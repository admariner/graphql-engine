use indexmap::IndexMap;
use open_dds::data_connector::CollectionName;
use open_dds::data_connector::DataConnectorColumnName;
use std::collections::BTreeMap;

use super::arguments;
use super::error;
use super::selection_set;
use crate::{CommandInfo, FunctionBasedCommand, ProcedureBasedCommand};
use open_dds::commands::ProcedureName;
use plan_types::{
    Argument, Field, FieldsSelection, JoinLocations, MutationExecutionPlan, NdcFieldAlias,
    NdcRelationshipName, PredicateQueryTrees, QueryExecutionPlan, QueryNodeNew, Relationship,
    VariableName, FUNCTION_IR_VALUE_COLUMN_NAME,
};

pub(crate) fn plan_query_node(
    ir: &CommandInfo<'_>,
    relationships: &mut BTreeMap<NdcRelationshipName, Relationship>,
) -> Result<(QueryNodeNew, JoinLocations), error::Error> {
    let mut ndc_nested_field = None;
    let mut jl = JoinLocations::new();
    if let Some(nested_selection) = &ir.selection {
        let (fields, locations) = selection_set::plan_nested_selection(
            nested_selection,
            ir.data_connector.capabilities.supported_ndc_version,
            relationships,
        )?;
        ndc_nested_field = Some(fields);
        jl = locations;
    }
    let query = QueryNodeNew {
        aggregates: None,
        fields: Some(FieldsSelection {
            fields: IndexMap::from([(
                NdcFieldAlias::from(FUNCTION_IR_VALUE_COLUMN_NAME),
                Field::Column {
                    column: DataConnectorColumnName::from(FUNCTION_IR_VALUE_COLUMN_NAME),
                    fields: ndc_nested_field,
                    arguments: BTreeMap::new(),
                },
            )]),
        }),
        limit: None,
        offset: None,
        order_by: None,
        predicate: None,
    };
    Ok((query, jl))
}

pub(crate) fn plan_query_execution(
    ir: &FunctionBasedCommand<'_>,
) -> Result<(QueryExecutionPlan, JoinLocations), error::Error> {
    let mut collection_relationships = BTreeMap::new();
    let mut arguments =
        arguments::plan_arguments(&ir.command_info.arguments, &mut collection_relationships)?;

    // Add the variable arguments which are used for remote joins
    for (variable_name, variable_argument) in &ir.variable_arguments {
        arguments.insert(
            variable_name.clone(),
            Argument::Variable {
                name: VariableName(variable_argument.clone()),
            },
        );
    }

    let (query_node, jl) = plan_query_node(&ir.command_info, &mut collection_relationships)?;

    let query_request = QueryExecutionPlan {
        remote_predicates: PredicateQueryTrees::new(),
        query_node,
        collection: CollectionName::from(ir.function_name.as_str()),
        arguments: arguments.clone(),
        collection_relationships,
        variables: None,
        data_connector: ir.command_info.data_connector.clone(),
    };
    Ok((query_request, jl))
}

pub(crate) fn plan_mutation_execution(
    procedure_name: &ProcedureName,
    ir: &ProcedureBasedCommand<'_>,
) -> Result<(MutationExecutionPlan, JoinLocations), error::Error> {
    let mut ndc_nested_field = None;
    let mut jl = JoinLocations::new();
    let mut collection_relationships = BTreeMap::new();
    if let Some(nested_selection) = &ir.command_info.selection {
        let (fields, locations) = selection_set::plan_nested_selection(
            nested_selection,
            ir.command_info
                .data_connector
                .capabilities
                .supported_ndc_version,
            &mut collection_relationships,
        )?;
        ndc_nested_field = Some(fields);
        jl = locations;
    }
    let mutation_request = MutationExecutionPlan {
        procedure_name: procedure_name.clone(),
        procedure_arguments: arguments::plan_mutation_arguments(
            &ir.command_info.arguments,
            &mut collection_relationships,
        )?,
        procedure_fields: ndc_nested_field,
        collection_relationships,
        data_connector: ir.command_info.data_connector.clone(),
    };
    Ok((mutation_request, jl))
}