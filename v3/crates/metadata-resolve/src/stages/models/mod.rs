use open_dds::identifier::SubgraphName;
use std::sync::Arc;
pub use types::{
    Model, ModelOrderBy, ModelRaw, ModelSource, ModelsIssue, ModelsOutput, NDCFieldSourceMapping,
};
mod aggregation;
mod error;
mod helpers;
mod source;
mod types;
pub use error::ModelsError;

pub use crate::helpers::argument::get_argument_kind;
pub use aggregation::resolve_aggregate_expression;
pub use helpers::get_ndc_column_for_comparison;

use crate::stages::{
    aggregates, apollo, boolean_expressions, data_connector_scalar_types, data_connectors, relay,
    scalar_types, type_permissions,
};
use crate::types::subgraph::{ArgumentInfo, Qualified, mk_qualified_type_reference};
use indexmap::IndexMap;
use open_dds::{
    aggregates::AggregateExpressionName, data_connector::DataConnectorName, models::ModelName,
    types::CustomTypeName,
};
use std::collections::BTreeMap;

pub fn resolve(
    metadata_accessor: &open_dds::accessor::MetadataAccessor,
    data_connectors: &data_connectors::DataConnectors,
    data_connector_scalars: &BTreeMap<
        Qualified<DataConnectorName>,
        data_connector_scalar_types::DataConnectorScalars,
    >,
    mut global_id_enabled_types: BTreeMap<Qualified<CustomTypeName>, Vec<Qualified<ModelName>>>,
    mut apollo_federation_entity_enabled_types: BTreeMap<
        Qualified<CustomTypeName>,
        Option<Qualified<open_dds::models::ModelName>>,
    >,
    object_types: &type_permissions::ObjectTypesWithPermissions,
    scalar_types: &BTreeMap<Qualified<CustomTypeName>, scalar_types::ScalarTypeRepresentation>,
    boolean_expression_types: &boolean_expressions::BooleanExpressionTypes,
    aggregate_expressions: &BTreeMap<
        Qualified<AggregateExpressionName>,
        aggregates::AggregateExpression,
    >,
) -> Result<ModelsOutput, Vec<ModelsError>> {
    let mut models = IndexMap::new();
    let mut global_id_models = BTreeMap::new();
    let mut issues = vec![];
    let mut results = vec![];

    for open_dds::accessor::QualifiedObject {
        path,
        subgraph,
        object: model,
    } in &metadata_accessor.models
    {
        results.push(process_model(
            path,
            subgraph,
            model,
            object_types,
            boolean_expression_types,
            data_connectors,
            data_connector_scalars,
            scalar_types,
            aggregate_expressions,
            &mut models,
            &mut global_id_enabled_types,
            &mut apollo_federation_entity_enabled_types,
            &mut global_id_models,
            &mut issues,
        ));
    }

    partition_eithers::collect_any_errors(results).map(|_| ModelsOutput {
        models,
        global_id_enabled_types,
        apollo_federation_entity_enabled_types,
        issues,
    })
}

/// resolve models and their sources
/// we check aggregations, filtering and graphql in the next stage (`models_graphql`)
fn process_model(
    path: &jsonpath::JSONPath,
    subgraph: &SubgraphName,
    model: &open_dds::models::Model,
    object_types: &type_permissions::ObjectTypesWithPermissions,
    boolean_expression_types: &boolean_expressions::BooleanExpressionTypes,
    data_connectors: &data_connectors::DataConnectors,
    data_connector_scalars: &BTreeMap<
        Qualified<DataConnectorName>,
        data_connector_scalar_types::DataConnectorScalars,
    >,
    scalar_types: &BTreeMap<Qualified<CustomTypeName>, scalar_types::ScalarTypeRepresentation>,
    aggregate_expressions: &BTreeMap<
        Qualified<AggregateExpressionName>,
        aggregates::AggregateExpression,
    >,
    models: &mut IndexMap<Qualified<ModelName>, Model>,
    global_id_enabled_types: &mut BTreeMap<Qualified<CustomTypeName>, Vec<Qualified<ModelName>>>,
    apollo_federation_entity_enabled_types: &mut BTreeMap<
        Qualified<CustomTypeName>,
        Option<Qualified<ModelName>>,
    >,
    global_id_models: &mut BTreeMap<Qualified<CustomTypeName>, Qualified<ModelName>>,
    issues: &mut Vec<ModelsIssue>,
) -> Result<(), ModelsError> {
    let qualified_model_name = Qualified::new(subgraph.clone(), model.name().clone());
    let mut resolved_model = resolve_model(
        path.clone(),
        subgraph,
        model,
        object_types,
        boolean_expression_types,
        global_id_enabled_types,
        apollo_federation_entity_enabled_types,
    )?;

    if resolved_model.global_id_source.is_some() {
        match global_id_models.insert(
            resolved_model.data_type.clone(),
            resolved_model.name.clone(),
        ) {
            None => {}
            Some(duplicate_model_name) => {
                Err(ModelsError::from(
                    relay::RelayError::DuplicateModelGlobalIdSource {
                        model_1: resolved_model.name.clone(),
                        model_2: duplicate_model_name,
                        object_type: resolved_model.data_type.clone(),
                    },
                ))?;
            }
        }
    }

    if let Some(model_source) = &model.source() {
        let (resolved_model_source, model_source_issues) = source::resolve_model_source(
            model_source,
            &mut resolved_model,
            subgraph,
            data_connectors,
            data_connector_scalars,
            object_types,
            scalar_types,
            boolean_expression_types,
        )?;
        resolved_model.source = Some(Arc::new(resolved_model_source));
        issues.extend(model_source_issues);
    }

    let qualified_aggregate_expression_name = model
        .aggregate_expression()
        .as_ref()
        .map(|aggregate_expression_name| {
            aggregation::resolve_aggregate_expression(
                &Qualified::new(subgraph.clone(), aggregate_expression_name.clone()),
                &qualified_model_name,
                &resolved_model.data_type,
                resolved_model.source.as_ref(),
                aggregate_expressions,
                object_types,
                data_connector_scalars,
            )
        })
        .transpose()?;

    resolved_model.aggregate_expression = qualified_aggregate_expression_name;

    if models
        .insert(qualified_model_name.clone(), resolved_model)
        .is_some()
    {
        Err(ModelsError::DuplicateModelDefinition {
            name: qualified_model_name,
        })?;
    }

    Ok(())
}

fn resolve_model(
    path: jsonpath::JSONPath,
    subgraph: &SubgraphName,
    model: &open_dds::models::Model,
    object_types: &type_permissions::ObjectTypesWithPermissions,
    boolean_expression_types: &boolean_expressions::BooleanExpressionTypes,
    global_id_enabled_types: &mut BTreeMap<Qualified<CustomTypeName>, Vec<Qualified<ModelName>>>,
    apollo_federation_entity_enabled_types: &mut BTreeMap<
        Qualified<CustomTypeName>,
        Option<Qualified<ModelName>>,
    >,
) -> Result<Model, ModelsError> {
    let qualified_object_type_name =
        Qualified::new(subgraph.clone(), model.object_type().value.clone());
    let qualified_model_name = Qualified::new(subgraph.clone(), model.name().clone());
    let object_type_representation = source::get_model_object_type_representation(
        object_types,
        &qualified_object_type_name,
        &qualified_model_name,
        &model.object_type().path,
    )?;
    let mut global_id_source = None;
    if model.global_id_source() {
        // Check if there are any global fields present in the related
        // object type, if the model is marked as a global source.
        if object_type_representation
            .object_type
            .global_id_fields
            .is_empty()
        {
            Err(ModelsError::from(
                relay::RelayError::NoGlobalFieldsPresentInGlobalIdSource {
                    type_name: qualified_object_type_name.clone(),
                    model_name: model.name().clone(),
                },
            ))?;
        }
        if !model.arguments().is_empty() {
            Err(ModelsError::from(
                relay::RelayError::ModelWithArgumentsAsGlobalIdSource {
                    model_name: qualified_model_name.clone(),
                },
            ))?;
        }
        // model has `global_id_source`; insert into the BTreeMap of `global_id_enabled_types`
        match global_id_enabled_types.get_mut(&qualified_object_type_name) {
            None => {
                // this shouldn't happen; but for some reason the object type
                // containing globalIdFields is not inserted. Insert it now
                global_id_enabled_types.insert(
                    qualified_object_type_name.clone(),
                    vec![qualified_model_name.clone()],
                );
            }
            Some(model_names) => {
                model_names.push(qualified_model_name.clone());
            }
        }
        global_id_source = Some(NDCFieldSourceMapping {
            ndc_mapping: BTreeMap::new(),
        });
    }
    let mut apollo_federation_key_source = None;
    let graphql = model.graphql();
    if graphql
        .as_ref()
        .and_then(|g| g.apollo_federation.as_ref().map(|a| a.entity_source))
        .unwrap_or_default()
    {
        // Check if there are any apollo federation keys present in the related
        // object type, if the model is marked as an apollo federation entity source.
        if object_type_representation
            .object_type
            .apollo_federation_config
            .is_some()
        {
            if !model.arguments().is_empty() {
                Err(ModelsError::from(
                    apollo::ApolloError::ModelWithArgumentsAsApolloFederationEntitySource {
                        model_name: qualified_model_name.clone(),
                    },
                ))?;
            }
            // model has `apollo_federation_entity_source`; insert into the BTreeMap of
            // `apollo_federation_entity_enabled_types`
            match apollo_federation_entity_enabled_types.get_mut(&qualified_object_type_name) {
                None => {
                    // the model's graphql configuration has `apollo_federation.entitySource` but the object type
                    // of the model doesn't have any apollo federation keys
                    Err(ModelsError::from(
                        apollo::ApolloError::NoKeysFieldsPresentInEntitySource {
                            type_name: qualified_object_type_name.clone(),
                            model_name: model.name().clone(),
                        },
                    ))?;
                }
                Some(type_name) => {
                    match type_name {
                        None => {
                            *type_name = Some(qualified_model_name.clone());
                        }
                        // Multiple models are marked as apollo federation entity source
                        Some(_) => {
                            Err(ModelsError::from(
                                apollo::ApolloError::MultipleEntitySourcesForType {
                                    type_name: qualified_object_type_name.clone(),
                                },
                            ))?;
                        }
                    }
                }
            }
            apollo_federation_key_source = Some(NDCFieldSourceMapping {
                ndc_mapping: BTreeMap::new(),
            });
        }
    }

    let mut arguments = IndexMap::new();
    for argument in model.arguments() {
        // is this an expression or not?
        let argument_kind =
            get_argument_kind(&argument.argument_type, subgraph, boolean_expression_types);

        if arguments
            .insert(
                argument.name.value.clone(),
                ArgumentInfo {
                    argument_type: mk_qualified_type_reference(&argument.argument_type, subgraph),
                    argument_kind,
                    description: argument.description.clone(),
                    path: argument.name.path.clone(),
                },
            )
            .is_some()
        {
            Err(ModelsError::DuplicateModelArgumentDefinition {
                model_name: qualified_model_name.clone(),
                argument_name: argument.name.clone(),
            })?;
        }
    }

    // keep relevant order by information for further resolving in `models_graphql`
    let order_by = match model {
        open_dds::models::Model::V1(model_v1) => ModelOrderBy::ModelV1 {
            orderable_fields: model_v1.orderable_fields.clone(),
            graphql_type_name: model_v1
                .graphql
                .as_ref()
                .and_then(|g| g.order_by_expression_type.clone()),
        },
        open_dds::models::Model::V2(model_v2) => {
            ModelOrderBy::ModelV2(model_v2.order_by_expression.clone())
        }
    };

    let model_raw = ModelRaw {
        description: model.description().clone(),
        filter_expression_type: model
            .filter_expression_type()
            .as_ref()
            .map(|filter_name| Qualified::new(subgraph.clone(), filter_name.clone())),
        order_by,
        graphql,
    };

    Ok(Model {
        path,
        name: qualified_model_name,
        data_type: qualified_object_type_name,
        data_type_path: model.object_type().path.clone(),
        aggregate_expression: None, // we fill this in once we have resolved the model source
        type_fields: object_type_representation.object_type.fields.clone(),
        global_id_fields: object_type_representation
            .object_type
            .global_id_fields
            .clone(),
        arguments,
        source: None,
        global_id_source,
        apollo_federation_key_source,
        raw: model_raw,
    })
}
