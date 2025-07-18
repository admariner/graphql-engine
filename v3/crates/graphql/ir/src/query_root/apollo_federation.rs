use std::collections::BTreeSet;
use std::collections::{BTreeMap, HashMap};

use hasura_authn_core::Session;
use indexmap::IndexMap;
use lang_graphql::{ast::common as ast, normalized_ast};
use open_dds::identifier;
use open_dds::types::CustomTypeName;
use open_dds::types::FieldName;

use crate::error;
use crate::flags::GraphqlIrFlags;
use crate::model_selection;
use graphql_schema::GDS;
use graphql_schema::{EntityFieldTypeNameMapping, NamespaceAnnotation};
use metadata_resolve;
use metadata_resolve::Qualified;
use metadata_resolve::mk_name;
use plan_types::UsagesCounts;

/// IR for the '_entities' operation for a model
#[derive(Debug)]
pub struct EntitySelect<'n, 's> {
    // The name of the field as published in the schema
    pub field_name: &'n ast::Name,

    /// Model Selection IR fragment
    pub model_selection: open_dds::query::ModelSelection,

    // We need this for validating the response from the data connector. This is not a reference as it is constructed
    // from the original selection set by filtering fields that are relevant.
    pub selection_set: normalized_ast::SelectionSet<'s, GDS>,

    // All the models/commands used in this operation. This includes the models/commands used via relationships. And in
    // future, the models/commands used in the filter clause
    pub(crate) usage_counts: UsagesCounts,
}

fn get_entity_namespace_typename_mappings<'s>(
    field_call: &normalized_ast::FieldCall<'s, GDS>,
) -> Result<&'s BTreeSet<Qualified<CustomTypeName>>, error::Error> {
    field_call
        .info
        .namespaced
        .as_ref()
        .and_then(|annotation| match annotation.as_ref() {
            NamespaceAnnotation::EntityTypeMappings(type_mappings) => Some(type_mappings),
            _ => None,
        })
        .ok_or(error::Error::Internal(error::InternalError::Engine(
            error::InternalEngineError::ExpectedNamespaceAnnotationNotFound {
                namespace_annotation_type: "Entity type mappings".to_string(),
            },
        )))
}

/// Generate the NDC IR for the entities root field.
///
/// This function generates the NDC IR for the entities root field. The entities query looks something like:
///
/// ```graphql
/// query MyQuery($representations: [_Any!]!) {
///   _entities(representations: $representations) {
///     ... on album {
///       AlbumId
///     }
///     ... on Article {
///       id
///     }
///   }
/// }
/// ```
/// The `representations` argument is a list of objects with a `__typename` field and the fields that are used to filter
/// the entities. The `__typename` field is used to determine the type of the entity and the fields are used to filter
/// the entities.
pub(crate) fn entities_ir<'n, 's>(
    field: &'n normalized_ast::Field<'s, GDS>,
    field_call: &'n normalized_ast::FieldCall<'s, GDS>,
    typename_mappings: &'s HashMap<ast::TypeName, EntityFieldTypeNameMapping>,
    models: &'s IndexMap<
        metadata_resolve::Qualified<open_dds::models::ModelName>,
        metadata_resolve::ModelWithPermissions,
    >,
    object_types: &'s BTreeMap<
        metadata_resolve::Qualified<open_dds::types::CustomTypeName>,
        metadata_resolve::ObjectTypeWithRelationships,
    >,
    session: &Session,
    request_headers: &reqwest::header::HeaderMap,
    flags: &GraphqlIrFlags,
) -> Result<Vec<EntitySelect<'n, 's>>, error::Error> {
    let representations = field_call
        .expected_argument(&lang_graphql::mk_name!("representations"))?
        .value
        .as_list()?;
    let mut entity_selects = vec![];
    for representation in representations {
        let json_representation = representation.as_json();
        let representation = json_representation.as_object().ok_or(
            lang_graphql::normalized_ast::Error::UnexpectedValue {
                expected_kind: "OBJECT",
                found: json_representation.clone(),
            },
        )?;
        // The __typename field is used to determine the type of the entity
        let typename_value = representation.get("__typename").ok_or(
            error::Error::FieldNotFoundInEntityRepresentation {
                field_name: FieldName::new(identifier!("__typename")),
            },
        )?;
        let typename_str = typename_value.as_str().ok_or(
            lang_graphql::normalized_ast::Error::UnexpectedValue {
                expected_kind: "STRING",
                found: typename_value.clone(),
            },
        )?;

        let typename = ast::TypeName(mk_name(typename_str).map_err(|_| {
            error::Error::TypeFieldInvalidGraphQlName {
                name: typename_str.to_string(),
            }
        })?);

        // Get the permissions for the typename
        let typename_permissions: &'s BTreeSet<Qualified<CustomTypeName>> =
            get_entity_namespace_typename_mappings(field_call)?;

        let typename_mapping = typename_mappings.get(&typename).ok_or(
            error::InternalDeveloperError::TypenameMappingNotFound {
                type_name: typename.clone(),
                mapping_kind: "entity key field",
            },
        )?;
        let role_model_select_permission = typename_permissions.get(&typename_mapping.type_name);
        // If the typename has a permission, then we can proceed to generate the NDC IR for the entity
        if let Some(_role_model_select_permission) = role_model_select_permission {
            // Get the model source for the entity
            let model_source = typename_mapping.model_source.as_ref().ok_or(
                error::InternalDeveloperError::NoSourceDataConnector {
                    type_name: typename.clone(),
                    field_name: lang_graphql::mk_name!("_entities"),
                },
            )?;
            let mut usage_counts = UsagesCounts::new();

            // Filter the selection set to only include fields that are relevant to the entity
            let new_selection_set = field.selection_set.filter_field_calls_by_typename(typename);

            let filter_clause_expressions: Vec<_> = typename_mapping
                .key_fields_ndc_mapping
                .keys()
                .map(|field_name| {
                    // Get the value of the field from the representation
                    let val = representation.get(field_name.as_str()).ok_or(
                        error::Error::FieldNotFoundInEntityRepresentation {
                            field_name: field_name.clone(),
                        },
                    )?;
                    Ok(open_dds::query::BooleanExpression::Comparison {
                        operand: open_dds::query::Operand::Field(
                            open_dds::query::ObjectFieldOperand {
                                nested: None,
                                target: Box::new(open_dds::query::ObjectFieldTarget {
                                    arguments: IndexMap::new(),
                                    field_name: field_name.clone(),
                                }),
                            },
                        ),
                        operator: open_dds::query::ComparisonOperator::Equals,
                        argument: Box::new(open_dds::query::Value::Literal(val.clone())),
                    })
                })
                .collect::<Result<_, error::Error>>()?;

            let filter = if filter_clause_expressions.is_empty() {
                None
            } else {
                Some(open_dds::query::BooleanExpression::And(
                    filter_clause_expressions,
                ))
            };

            let model_selection = model_selection::model_selection_open_dd_ir(
                &new_selection_set,
                &typename_mapping.model_name,
                models,
                &model_source.type_mappings,
                object_types,
                None, // arguments
                filter,
                vec![], // order_by
                None,   // limit
                None,   // offset
                &session.variables,
                request_headers,
                flags,
                &mut usage_counts,
            )?;

            entity_selects.push(EntitySelect {
                field_name: &field_call.name,
                model_selection,
                selection_set: new_selection_set,
                usage_counts,
            });
        }
    }
    Ok(entity_selects)
}
