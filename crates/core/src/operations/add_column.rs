//! Add a new column to a table

use std::sync::Arc;

use delta_kernel::schema::StructType;
use futures::future::BoxFuture;
use itertools::Itertools;

use super::transaction::{CommitBuilder, CommitProperties, PROTOCOL};
use super::{CustomExecuteHandler, Operation};
use crate::kernel::StructField;
use crate::logstore::LogStoreRef;
use crate::operations::cast::merge_schema::merge_delta_struct;
use crate::protocol::DeltaOperation;
use crate::table::state::DeltaTableState;
use crate::{DeltaResult, DeltaTable, DeltaTableError};

/// Add new columns and/or nested fields to a table
pub struct AddColumnBuilder {
    /// A snapshot of the table's state
    snapshot: DeltaTableState,
    /// Fields to add/merge into schema
    fields: Option<Vec<StructField>>,
    /// Delta object store for handling data files
    log_store: LogStoreRef,
    /// Additional information to add to the commit
    commit_properties: CommitProperties,
    custom_execute_handler: Option<Arc<dyn CustomExecuteHandler>>,
}

impl super::Operation<()> for AddColumnBuilder {
    fn log_store(&self) -> &LogStoreRef {
        &self.log_store
    }
    fn get_custom_execute_handler(&self) -> Option<Arc<dyn CustomExecuteHandler>> {
        self.custom_execute_handler.clone()
    }
}

impl AddColumnBuilder {
    /// Create a new builder
    pub fn new(log_store: LogStoreRef, snapshot: DeltaTableState) -> Self {
        Self {
            snapshot,
            log_store,
            fields: None,
            commit_properties: CommitProperties::default(),
            custom_execute_handler: None,
        }
    }

    /// Specify the fields to be added
    pub fn with_fields(mut self, fields: impl IntoIterator<Item = StructField> + Clone) -> Self {
        self.fields = Some(fields.into_iter().collect());
        self
    }
    /// Additional metadata to be added to commit info
    pub fn with_commit_properties(mut self, commit_properties: CommitProperties) -> Self {
        self.commit_properties = commit_properties;
        self
    }

    /// Set a custom execute handler, for pre and post execution
    pub fn with_custom_execute_handler(mut self, handler: Arc<dyn CustomExecuteHandler>) -> Self {
        self.custom_execute_handler = Some(handler);
        self
    }
}

impl std::future::IntoFuture for AddColumnBuilder {
    type Output = DeltaResult<DeltaTable>;

    type IntoFuture = BoxFuture<'static, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        let this = self;

        Box::pin(async move {
            let mut metadata = this.snapshot.metadata().clone();
            let fields = match this.fields.clone() {
                Some(v) => v,
                None => return Err(DeltaTableError::Generic("No fields provided".to_string())),
            };
            let operation_id = this.get_operation_id();
            this.pre_execute(operation_id).await?;

            let fields_right = &StructType::new(fields.clone());
            let table_schema = this.snapshot.schema();
            let new_table_schema = merge_delta_struct(table_schema, fields_right)?;

            // TODO(ion): Think of a way how we can simply this checking through the API or centralize some checks.
            let contains_timestampntz = PROTOCOL.contains_timestampntz(fields.iter());
            let protocol = this.snapshot.protocol();

            let maybe_new_protocol = if contains_timestampntz {
                let updated_protocol = protocol.clone().enable_timestamp_ntz();
                if !(protocol.min_reader_version == 3 && protocol.min_writer_version == 7) {
                    // Convert existing properties to features since we advanced the protocol to v3,7
                    Some(
                        updated_protocol
                            .move_table_properties_into_features(&metadata.configuration),
                    )
                } else {
                    Some(updated_protocol)
                }
            } else {
                None
            };

            let operation = DeltaOperation::AddColumn {
                fields: fields.into_iter().collect_vec(),
            };

            metadata.schema_string = serde_json::to_string(&new_table_schema)?;

            let mut actions = vec![metadata.into()];

            if let Some(new_protocol) = maybe_new_protocol {
                actions.push(new_protocol.into())
            }

            let commit = CommitBuilder::from(this.commit_properties.clone())
                .with_actions(actions)
                .with_operation_id(operation_id)
                .with_post_commit_hook_handler(this.get_custom_execute_handler())
                .build(Some(&this.snapshot), this.log_store.clone(), operation)
                .await?;

            this.post_execute(operation_id).await?;

            Ok(DeltaTable::new_with_state(
                this.log_store,
                commit.snapshot(),
            ))
        })
    }
}
