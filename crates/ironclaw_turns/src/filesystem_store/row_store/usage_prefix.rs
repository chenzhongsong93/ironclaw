use super::{
    FilesystemTurnStateRowStore, RUN_ROWS,
    delta::SnapshotDelta,
    io::{delta_log_path, deserialize_materialized_row, fs_error, row_path},
};
use crate::run_tree_usage::{RunTreeUsagePrefixSource, RunTreeUsageSession, RunUsagePrefix};
use crate::{TurnError, TurnRunId, TurnRunRecord, TurnScope};
use async_trait::async_trait;
use ironclaw_filesystem::RootFilesystem;
use ironclaw_host_api::ResourceScope;
fn not_ready(reason: &'static str) -> TurnError {
    TurnError::Unavailable {
        reason: reason.to_string(),
    }
}
#[async_trait]
impl<F: RootFilesystem + Send + Sync + 'static> RunTreeUsagePrefixSource
    for FilesystemTurnStateRowStore<F>
{
    async fn current_usage_log_generation(&self) -> Result<String, TurnError> {
        self.ensure_not_degraded().await?;
        self.filesystem
            .event_log_generation(&ResourceScope::system(), &delta_log_path()?)
            .await
            .map_err(fs_error)?
            .ok_or(TurnError::ScopeNotFound)
    }
    async fn open_usage_tree(
        &self,
        root_scope: TurnScope,
        root_run_id: TurnRunId,
    ) -> Result<RunTreeUsageSession, TurnError> {
        // One original backend committed statement/MVCC view, even on cold open.
        // Does not initialize/load/materialize/flush/migrate/repair anything,
        // does not observe/take/drop original native ACKs or extend any clock.
        let snapshot = {
            let _guard = self.snapshot_state.lock().await;
            self.ensure_not_degraded().await?;
            self.filesystem
                .complete_event_prefix(
                    &ResourceScope::system(),
                    &delta_log_path()?,
                    100_000,
                    64 * 1024 * 1024,
                    8 * 1024 * 1024,
                )
                .await
                .map_err(fs_error)?
        };
        let Some(snapshot) = snapshot else {
            return self.missing_usage_root(&root_scope, root_run_id).await;
        };
        let h = snapshot.head().get();
        let generation = snapshot.generation().to_string();
        let mut prefix = RunUsagePrefix::new();
        for record in snapshot.into_records() {
            let delta: SnapshotDelta = serde_json::from_slice(&record.payload)
                .map_err(|_| not_ready("invalid original durable run delta"))?;
            prefix.ingest(
                record.seq.get(),
                delta.runs_upsert,
                &delta.events_upsert,
                !delta.runs_delete.is_empty(),
            )?;
        }
        self.ensure_not_degraded().await?;
        if !prefix.contains_run(&root_run_id) {
            return self.missing_usage_root(&root_scope, root_run_id).await;
        }
        // New same-path appends are allowed; only a reset/deletion invalidates
        // this log-incarnation view. Page adapter rechecks this original token.
        if self.current_usage_log_generation().await? != generation {
            return Err(not_ready("run journal incarnation changed"));
        }
        prefix.finish(root_scope, root_run_id, h, generation)
    }
}
impl<F: RootFilesystem + Send + Sync + 'static> FilesystemTurnStateRowStore<F> {
    async fn missing_usage_root(
        &self,
        scope: &TurnScope,
        id: TurnRunId,
    ) -> Result<RunTreeUsageSession, TurnError> {
        // Pure point read only: retained row without its original birth prefix
        // is NOT a complete root, and must not be confused with a missing root.
        let row = self
            .filesystem
            .get(
                &ResourceScope::system(),
                &row_path(RUN_ROWS, &id.to_string())?,
            )
            .await
            .map_err(fs_error)?;
        if let Some(row) = row {
            let actual: Option<TurnRunRecord> =
                deserialize_materialized_row(&row.entry.body, "runs")?;
            if actual.as_ref().is_some_and(|r| r.scope == *scope) {
                return Err(not_ready(
                    "root retained but original journal coverage missing",
                ));
            }
        }
        Err(TurnError::ScopeNotFound)
    }
}

#[async_trait]
impl<F: RootFilesystem + Send + Sync + 'static> RunTreeUsagePrefixSource
    for crate::FilesystemTurnStateStoreKind<F>
{
    async fn current_usage_log_generation(&self) -> Result<String, TurnError> {
        match self {
            Self::Row(store) => store.current_usage_log_generation().await,
            Self::Blob(_) => Err(not_ready(
                "complete event-prefix usage unsupported by blob authority",
            )),
        }
    }
    async fn open_usage_tree(
        &self,
        scope: TurnScope,
        root: TurnRunId,
    ) -> Result<RunTreeUsageSession, TurnError> {
        match self {
            Self::Row(store) => store.open_usage_tree(scope, root).await,
            Self::Blob(_) => Err(not_ready(
                "complete event-prefix usage unsupported by blob authority",
            )),
        }
    }
}
