use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use ironclaw_filesystem::{
    CasExpectation, ContentType, Entry, FilesystemError, RootFilesystem, ScopedFilesystem, SeqNo,
};
use ironclaw_host_api::ResourceScope;
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc, oneshot},
    task::JoinHandle,
};

use crate::TurnError;

use super::{
    ACTIVE_LOCK_ROWS, ADMISSION_RESERVATION_ROWS, CHECKPOINT_ROWS, EVENT_ROWS, IDEMPOTENCY_ROWS,
    LOOP_CHECKPOINT_ROWS, RUN_ROWS, SPAWN_TREE_RESERVATION_ROWS, TURN_ROWS,
    delta::{
        RowStoreMeta, SnapshotDelta, active_lock_record_key, admission_reservation_record_key,
        checkpoint_record_key, event_record_key, idempotency_record_key,
        loop_checkpoint_record_key, run_record_key, spawn_tree_reservation_record_key,
        turn_record_key,
    },
    io::{
        delta_log_path, deserialize_row, fs_error, materialized_row_seq, meta_path, row_path,
        serialize_materialized_row,
    },
};

const DELTA_JOURNAL_MAX_BATCH: usize = 256;
const DELTA_JOURNAL_FLUSH_COALESCE_DELAY: Duration = Duration::from_micros(500);
const DELTA_JOURNAL_MATERIALIZE_IDLE_DELAY: Duration = Duration::from_millis(25);
const MATERIALIZED_ROW_CAS_RETRIES: usize = 16;

/// Backend contention (`FilesystemError::BackendBusy`) never commits the
/// attempted append (the backend contract for the event plane is
/// all-or-nothing), so retrying the identical batch cannot create a durable
/// gap. The flusher retries busy appends with exponential backoff BEFORE
/// latching the store degraded: a single transient contention window must not
/// permanently wedge the whole turn engine (TianQuan ISSUE-IRONCLAW-002 — two
/// production incidents where one busy append halted every subsequent
/// mutation until a manual restart).
const DELTA_JOURNAL_BUSY_RETRY_MAX_ATTEMPTS: u32 = 5;
const DELTA_JOURNAL_BUSY_RETRY_BASE_DELAY: Duration = Duration::from_millis(100);
const DELTA_JOURNAL_BUSY_RETRY_MAX_DELAY: Duration = Duration::from_millis(1600);

fn busy_backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(4);
    DELTA_JOURNAL_BUSY_RETRY_BASE_DELAY
        .saturating_mul(1u32 << shift)
        .min(DELTA_JOURNAL_BUSY_RETRY_MAX_DELAY)
}

pub(super) type DeltaAck = oneshot::Receiver<Result<(), TurnError>>;

pub(super) struct DeltaJournal {
    sender: mpsc::UnboundedSender<DeltaJournalRequest>,
    /// Latched `true` by the flusher when a write-behind append fails. The row
    /// store checks this at mutation entry to fail fast and to reload its hot
    /// cache from the last consistent durable point.
    degraded: Arc<AtomicBool>,
    /// Background task handles, aborted on drop so a "crashed" (dropped) store
    /// cannot keep appending queued write-behind deltas to a shared backend
    /// after the crash point.
    flusher: JoinHandle<()>,
    materializer: JoinHandle<()>,
}

impl Drop for DeltaJournal {
    fn drop(&mut self) {
        // Deterministic crash: stop the detached durable pipeline at the drop
        // point. Acked write-through data is already durable (the caller awaited
        // its ack before dropping the store); only queued-but-un-appended
        // write-behind deltas are lost — exactly the bounded crash-loss window.
        self.flusher.abort();
        self.materializer.abort();
    }
}

struct DeltaJournalRequest {
    delta: SnapshotDelta,
    ack: oneshot::Sender<Result<(), TurnError>>,
}

impl DeltaJournal {
    pub(super) fn new<F>(
        filesystem: Arc<ScopedFilesystem<F>>,
        materialize_gate: Arc<AsyncMutex<()>>,
    ) -> Self
    where
        F: RootFilesystem + 'static,
    {
        let (sender, receiver) = mpsc::unbounded_channel();
        let (materialize_sender, materialize_receiver) = mpsc::unbounded_channel();
        let degraded = Arc::new(AtomicBool::new(false));
        let materializer = tokio::spawn(run_delta_journal_materializer(
            Arc::clone(&filesystem),
            materialize_gate,
            materialize_receiver,
        ));
        let flusher = tokio::spawn(run_delta_journal_flusher(
            filesystem,
            receiver,
            materialize_sender,
            Arc::clone(&degraded),
        ));
        Self {
            sender,
            degraded,
            flusher,
            materializer,
        }
    }

    /// Whether the flusher has halted the durable sequence after a write-behind
    /// append failure. Once `true`, the store is degraded until reopened.
    pub(super) fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    pub(super) fn enqueue(&self, delta: SnapshotDelta) -> Result<Option<DeltaAck>, TurnError> {
        if delta.is_empty() {
            return Ok(None);
        }
        let (ack, receiver) = oneshot::channel();
        self.sender
            .send(DeltaJournalRequest { delta, ack })
            .map_err(|_| delta_journal_stopped())?;
        Ok(Some(receiver))
    }

    pub(super) async fn await_ack(ack: Option<DeltaAck>) -> Result<(), TurnError> {
        let Some(ack) = ack else {
            return Ok(());
        };
        ack.await.map_err(|_| delta_journal_stopped())?
    }

    /// Await an ack IN PLACE, by mutable reference, so a cancelled await leaves
    /// the ack owned by the caller (e.g. still tracked in the pending window)
    /// rather than consuming and dropping it. `DeltaAck` is a `oneshot::Receiver`
    /// — `Future + Unpin` — so `(&mut *ack).await` polls it without taking
    /// ownership; the caller removes it only after this resolves. Used by the
    /// write-behind backpressure reserve, which runs under an outer timeout that
    /// can cancel it (#6298 IronLoop f7).
    pub(super) async fn await_ack_ref(ack: &mut DeltaAck) -> Result<(), TurnError> {
        (&mut *ack).await.map_err(|_| delta_journal_stopped())?
    }
}

async fn run_delta_journal_flusher<F>(
    filesystem: Arc<ScopedFilesystem<F>>,
    mut receiver: mpsc::UnboundedReceiver<DeltaJournalRequest>,
    materialize_sender: mpsc::UnboundedSender<SeqNo>,
    degraded: Arc<AtomicBool>,
) where
    F: RootFilesystem,
{
    while let Some(first) = receiver.recv().await {
        let mut requests = Vec::with_capacity(DELTA_JOURNAL_MAX_BATCH);
        requests.push(first);
        tokio::time::sleep(DELTA_JOURNAL_FLUSH_COALESCE_DELAY).await;
        while requests.len() < DELTA_JOURNAL_MAX_BATCH {
            match receiver.try_recv() {
                Ok(request) => requests.push(request),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }
        let mut busy_attempt: u32 = 0;
        let result = loop {
            match append_delta_journal_batch(filesystem.as_ref(), &requests).await {
                Ok(seqs) => break Ok(seqs),
                Err(failure)
                    if failure.retryable_busy
                        && busy_attempt < DELTA_JOURNAL_BUSY_RETRY_MAX_ATTEMPTS =>
                {
                    busy_attempt += 1;
                    let delay = busy_backoff_delay(busy_attempt);
                    tracing::warn!(
                        attempt = busy_attempt,
                        max_attempts = DELTA_JOURNAL_BUSY_RETRY_MAX_ATTEMPTS,
                        delay_ms = delay.as_millis() as u64,
                        batch_len = requests.len(),
                        "delta journal append hit retryable backend contention; \
                         retrying the identical batch (busy appends never commit, \
                         so no durable gap is possible)",
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(failure) => break Err(failure),
            }
        };
        match result {
            Ok(seqs) => {
                let target_seq = seqs.iter().copied().max();
                for request in requests {
                    let _ = request.ack.send(Ok(()));
                }
                if let Some(target_seq) = target_seq {
                    let _ = materialize_sender.send(target_seq);
                }
            }
            Err(failure) => {
                for request in requests {
                    let _ = request.ack.send(Err(failure.error.clone()));
                }
                // Append-failure HALT. A non-critical op already returned `Ok`
                // to its caller when it enqueued, so CONTINUING to the next
                // batch would append later deltas AFTER a durable GAP the lost
                // op left — corruption on replay. Instead, latch the store
                // degraded and stop the flusher so nothing can append behind
                // the gap. The store then fails subsequent mutations fast and
                // reloads its hot cache from the last consistent durable point
                // (the accepted, recoverable crash-loss). Drain the remaining
                // queue with the halt error so a critical-op barrier parked on
                // its ack unblocks with a retryable error rather than hanging.
                degraded.store(true, Ordering::SeqCst);
                receiver.close();
                while let Ok(request) = receiver.try_recv() {
                    let _ = request.ack.send(Err(delta_journal_halted()));
                }
                return;
            }
        }
    }
}

async fn run_delta_journal_materializer<F>(
    filesystem: Arc<ScopedFilesystem<F>>,
    materialize_gate: Arc<AsyncMutex<()>>,
    mut receiver: mpsc::UnboundedReceiver<SeqNo>,
) where
    F: RootFilesystem,
{
    while let Some(mut target_seq) = receiver.recv().await {
        tokio::time::sleep(DELTA_JOURNAL_MATERIALIZE_IDLE_DELAY).await;
        while let Ok(next_seq) = receiver.try_recv() {
            target_seq = target_seq.max(next_seq);
        }
        if let Err(error) =
            materialize_delta_batch(filesystem.as_ref(), &materialize_gate, &target_seq).await
        {
            tracing::debug!(
                error = %error,
                target_seq = target_seq.get(),
                "turn-state row materialization failed after durable delta append",
            );
        }
    }
}

/// Why a delta-journal batch append failed, plus whether the flusher may
/// safely retry the identical batch.
struct JournalAppendFailure {
    error: TurnError,
    /// `true` only for [`FilesystemError::BackendBusy`]: the backend reported
    /// retryable contention and did NOT commit the batch, so retrying cannot
    /// create a durable gap. Every other failure keeps the conservative
    /// halt-and-degrade behavior (its commit status is unknown).
    retryable_busy: bool,
}

fn journal_append_failure(error: FilesystemError) -> JournalAppendFailure {
    let retryable_busy = matches!(error, FilesystemError::BackendBusy { .. });
    JournalAppendFailure {
        error: fs_error(error),
        retryable_busy,
    }
}

async fn append_delta_journal_batch<F>(
    filesystem: &ScopedFilesystem<F>,
    requests: &[DeltaJournalRequest],
) -> Result<Vec<SeqNo>, JournalAppendFailure>
where
    F: RootFilesystem,
{
    let path = delta_log_path().map_err(|error| JournalAppendFailure {
        error,
        retryable_busy: false,
    })?;
    let mut payloads = Vec::with_capacity(requests.len());
    for request in requests {
        payloads.push(serde_json::to_vec(&request.delta).map_err(|error| {
            JournalAppendFailure {
                error: TurnError::Unavailable {
                    reason: format!("turn-state delta serialization failed: {error}"),
                },
                retryable_busy: false,
            }
        })?);
    }
    let seqs = if let [payload] = payloads.as_slice() {
        vec![
            filesystem
                .append(&ResourceScope::system(), &path, payload.clone())
                .await
                .map_err(journal_append_failure)?,
        ]
    } else {
        filesystem
            .append_batch(&ResourceScope::system(), &path, payloads)
            .await
            .map_err(journal_append_failure)?
    };
    if seqs.len() != requests.len() {
        return Err(JournalAppendFailure {
            error: TurnError::Unavailable {
                reason: "turn-state delta batch append returned an unexpected ack count"
                    .to_string(),
            },
            retryable_busy: false,
        });
    }
    Ok(seqs)
}

async fn materialize_delta_batch<F>(
    filesystem: &ScopedFilesystem<F>,
    materialize_gate: &Arc<AsyncMutex<()>>,
    target_seq: &SeqNo,
) -> Result<(), TurnError>
where
    F: RootFilesystem,
{
    materialize_delta_log(filesystem, materialize_gate, Some(*target_seq)).await
}

pub(super) async fn materialize_delta_log<F>(
    filesystem: &ScopedFilesystem<F>,
    materialize_gate: &Arc<AsyncMutex<()>>,
    target_seq: Option<SeqNo>,
) -> Result<(), TurnError>
where
    F: RootFilesystem,
{
    let _guard = materialize_gate.lock().await;
    materialize_delta_log_unlocked(filesystem, target_seq).await
}

async fn materialize_delta_log_unlocked<F>(
    filesystem: &ScopedFilesystem<F>,
    target_seq: Option<SeqNo>,
) -> Result<(), TurnError>
where
    F: RootFilesystem,
{
    let mut meta = read_meta(filesystem).await?;
    if let Some(target_seq) = target_seq
        && target_seq <= meta.journal_seq
    {
        return Ok(());
    }
    let path = delta_log_path()?;
    let max_records = target_seq
        .map(|target_seq| target_seq.get().saturating_sub(meta.journal_seq.get()) as usize)
        .unwrap_or(usize::MAX);
    let records = match filesystem
        .tail_bounded(
            &ResourceScope::system(),
            &path,
            meta.journal_seq,
            max_records,
        )
        .await
    {
        Ok(records) => records,
        Err(FilesystemError::NotFound { .. }) | Err(FilesystemError::Unsupported { .. }) => {
            Vec::new()
        }
        Err(error) => return Err(fs_error(error)),
    };
    let mut updated = false;
    for record in records {
        if target_seq.is_some_and(|target_seq| record.seq > target_seq) {
            break;
        }
        let delta: SnapshotDelta =
            serde_json::from_slice(&record.payload).map_err(|error| TurnError::Unavailable {
                reason: format!("turn-state delta deserialization failed: {error}"),
            })?;
        materialize_delta(filesystem, record.seq, &delta).await?;
        if let Some(floor) = delta.event_retention_floor {
            meta.event_retention_floor = meta.event_retention_floor.max(floor);
        }
        meta.journal_seq = record.seq;
        updated = true;
    }
    if updated {
        write_meta(filesystem, &meta).await?;
    }
    Ok(())
}

async fn materialize_delta<F>(
    filesystem: &ScopedFilesystem<F>,
    journal_seq: SeqNo,
    delta: &SnapshotDelta,
) -> Result<(), TurnError>
where
    F: RootFilesystem,
{
    for record in &delta.turns_upsert {
        put_row(
            filesystem,
            TURN_ROWS,
            &turn_record_key(record)?,
            journal_seq,
            record,
        )
        .await?;
    }
    for key in &delta.turns_delete {
        delete_row(filesystem, TURN_ROWS, key, journal_seq).await?;
    }
    for record in &delta.runs_upsert {
        put_row(
            filesystem,
            RUN_ROWS,
            &run_record_key(record)?,
            journal_seq,
            record,
        )
        .await?;
    }
    for key in &delta.runs_delete {
        delete_row(filesystem, RUN_ROWS, key, journal_seq).await?;
    }
    for record in &delta.active_locks_upsert {
        put_row(
            filesystem,
            ACTIVE_LOCK_ROWS,
            &active_lock_record_key(record)?,
            journal_seq,
            record,
        )
        .await?;
    }
    for key in &delta.active_locks_delete {
        delete_row(filesystem, ACTIVE_LOCK_ROWS, key, journal_seq).await?;
    }
    for record in &delta.checkpoints_upsert {
        put_row(
            filesystem,
            CHECKPOINT_ROWS,
            &checkpoint_record_key(record)?,
            journal_seq,
            record,
        )
        .await?;
    }
    for key in &delta.checkpoints_delete {
        delete_row(filesystem, CHECKPOINT_ROWS, key, journal_seq).await?;
    }
    for record in &delta.loop_checkpoints_upsert {
        put_row(
            filesystem,
            LOOP_CHECKPOINT_ROWS,
            &loop_checkpoint_record_key(record)?,
            journal_seq,
            record,
        )
        .await?;
    }
    for key in &delta.loop_checkpoints_delete {
        delete_row(filesystem, LOOP_CHECKPOINT_ROWS, key, journal_seq).await?;
    }
    for record in &delta.idempotency_upsert {
        put_row(
            filesystem,
            IDEMPOTENCY_ROWS,
            &idempotency_record_key(record)?,
            journal_seq,
            record,
        )
        .await?;
    }
    for key in &delta.idempotency_delete {
        delete_row(filesystem, IDEMPOTENCY_ROWS, key, journal_seq).await?;
    }
    for record in &delta.events_upsert {
        put_row(
            filesystem,
            EVENT_ROWS,
            &event_record_key(record)?,
            journal_seq,
            record,
        )
        .await?;
    }
    for key in &delta.events_delete {
        delete_row(filesystem, EVENT_ROWS, key, journal_seq).await?;
    }
    for record in &delta.admission_reservations_upsert {
        put_row(
            filesystem,
            ADMISSION_RESERVATION_ROWS,
            &admission_reservation_record_key(record)?,
            journal_seq,
            record,
        )
        .await?;
    }
    for key in &delta.admission_reservations_delete {
        delete_row(filesystem, ADMISSION_RESERVATION_ROWS, key, journal_seq).await?;
    }
    for record in &delta.spawn_tree_reservations_upsert {
        put_row(
            filesystem,
            SPAWN_TREE_RESERVATION_ROWS,
            &spawn_tree_reservation_record_key(record)?,
            journal_seq,
            record,
        )
        .await?;
    }
    for key in &delta.spawn_tree_reservations_delete {
        delete_row(filesystem, SPAWN_TREE_RESERVATION_ROWS, key, journal_seq).await?;
    }
    Ok(())
}

async fn put_row<F, T>(
    filesystem: &ScopedFilesystem<F>,
    collection: &'static str,
    key: &str,
    journal_seq: SeqNo,
    record: &T,
) -> Result<(), TurnError>
where
    F: RootFilesystem,
    T: serde::Serialize,
{
    let body = serialize_materialized_row(journal_seq, Some(record), collection)?;
    write_materialized_row(filesystem, collection, key, journal_seq, body).await
}

async fn delete_row<F>(
    filesystem: &ScopedFilesystem<F>,
    collection: &'static str,
    key: &str,
    journal_seq: SeqNo,
) -> Result<(), TurnError>
where
    F: RootFilesystem,
{
    let body = serialize_materialized_row::<serde_json::Value>(journal_seq, None, collection)?;
    write_materialized_row(filesystem, collection, key, journal_seq, body).await
}

async fn write_materialized_row<F>(
    filesystem: &ScopedFilesystem<F>,
    collection: &'static str,
    key: &str,
    journal_seq: SeqNo,
    body: Vec<u8>,
) -> Result<(), TurnError>
where
    F: RootFilesystem,
{
    let path = row_path(collection, key)?;
    for attempt in 0..MATERIALIZED_ROW_CAS_RETRIES {
        let current = match filesystem.get(&ResourceScope::system(), &path).await {
            Ok(current) => current,
            Err(FilesystemError::NotFound { .. }) => None,
            Err(error) => return Err(fs_error(error)),
        };
        let cas = match current {
            Some(versioned) => {
                let current_seq = materialized_row_seq(&versioned.entry.body, collection)?;
                if current_seq >= journal_seq {
                    return Ok(());
                }
                CasExpectation::Version(versioned.version)
            }
            None => CasExpectation::Absent,
        };
        let entry = Entry::bytes(body.clone()).with_content_type(ContentType::json());
        match filesystem
            .put(&ResourceScope::system(), &path, entry, cas)
            .await
        {
            Ok(_version) => return Ok(()),
            Err(FilesystemError::VersionMismatch { .. })
                if attempt + 1 < MATERIALIZED_ROW_CAS_RETRIES =>
            {
                tokio::task::yield_now().await;
            }
            Err(error) => return Err(fs_error(error)),
        }
    }
    Err(TurnError::Unavailable {
        reason: format!("turn-state {collection} row CAS retry budget exhausted"),
    })
}

async fn read_meta<F>(filesystem: &ScopedFilesystem<F>) -> Result<RowStoreMeta, TurnError>
where
    F: RootFilesystem,
{
    match filesystem
        .get(&ResourceScope::system(), &meta_path()?)
        .await
    {
        Ok(Some(versioned)) => deserialize_row(&versioned.entry.body, "turn-state row meta"),
        Ok(None) | Err(FilesystemError::NotFound { .. }) => Ok(RowStoreMeta::default()),
        Err(error) => Err(fs_error(error)),
    }
}

async fn write_meta<F>(
    filesystem: &ScopedFilesystem<F>,
    meta: &RowStoreMeta,
) -> Result<(), TurnError>
where
    F: RootFilesystem,
{
    let path = meta_path()?;
    for attempt in 0..MATERIALIZED_ROW_CAS_RETRIES {
        let current = match filesystem.get(&ResourceScope::system(), &path).await {
            Ok(current) => current,
            Err(FilesystemError::NotFound { .. }) => None,
            Err(error) => return Err(fs_error(error)),
        };
        let (next, cas) = match current {
            Some(versioned) => {
                let current_meta: RowStoreMeta =
                    deserialize_row(&versioned.entry.body, "turn-state row meta")?;
                if current_meta.journal_seq >= meta.journal_seq
                    && current_meta.event_retention_floor >= meta.event_retention_floor
                {
                    return Ok(());
                }
                let mut next = meta.clone();
                next.journal_seq = next.journal_seq.max(current_meta.journal_seq);
                next.event_retention_floor = next
                    .event_retention_floor
                    .max(current_meta.event_retention_floor);
                (next, CasExpectation::Version(versioned.version))
            }
            None => (meta.clone(), CasExpectation::Absent),
        };
        let body = serde_json::to_vec(&next).map_err(|error| TurnError::Unavailable {
            reason: format!("turn-state row meta serialization failed: {error}"),
        })?;
        let entry = Entry::bytes(body).with_content_type(ContentType::json());
        match filesystem
            .put(&ResourceScope::system(), &path, entry, cas)
            .await
        {
            Ok(_version) => return Ok(()),
            Err(FilesystemError::VersionMismatch { .. })
                if attempt + 1 < MATERIALIZED_ROW_CAS_RETRIES =>
            {
                tokio::task::yield_now().await;
            }
            Err(error) => return Err(fs_error(error)),
        }
    }
    Err(TurnError::Unavailable {
        reason: "turn-state row meta CAS retry budget exhausted".to_string(),
    })
}

fn delta_journal_stopped() -> TurnError {
    TurnError::Unavailable {
        reason: "turn-state delta journal stopped".to_string(),
    }
}

fn delta_journal_halted() -> TurnError {
    TurnError::Unavailable {
        reason: "turn-state delta journal halted after a write-behind append failure".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use ironclaw_filesystem::{
        BackendCapabilities, DirEntry, EventRecord, FileStat, FilesystemOperation, InMemoryBackend,
        RecordVersion, ScopedFilesystem, VersionedEntry,
    };
    use ironclaw_host_api::{MountAlias, MountGrant, MountPermissions, MountView, VirtualPath};
    use serde::{Deserialize, Serialize};

    use super::super::io::{deserialize_materialized_row, row_path};
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct TestRow {
        value: String,
    }

    fn scoped_filesystem() -> ScopedFilesystem<InMemoryBackend> {
        let mounts = MountView::new(vec![MountGrant::new(
            MountAlias::new("/turns").expect("create turns mount alias"),
            VirtualPath::new("/turns").expect("create turns virtual path"),
            MountPermissions::read_write_list_delete(),
        )])
        .expect("create turns mount view");
        ScopedFilesystem::with_fixed_view(Arc::new(InMemoryBackend::new()), mounts)
    }

    async fn read_test_row(
        filesystem: &ScopedFilesystem<InMemoryBackend>,
        key: &str,
    ) -> Option<TestRow> {
        let path = row_path("test-rows", key).expect("create row path");
        let versioned = filesystem
            .get(&ResourceScope::system(), &path)
            .await
            .expect("read test row")?;
        deserialize_materialized_row(&versioned.entry.body, "test-rows")
            .expect("deserialize test row")
    }

    #[tokio::test]
    async fn materialized_row_write_skips_older_journal_sequence() {
        let filesystem = scoped_filesystem();
        put_row(
            &filesystem,
            "test-rows",
            "row",
            SeqNo::from_backend(2),
            &TestRow {
                value: "newer".to_string(),
            },
        )
        .await
        .expect("write newer row");

        put_row(
            &filesystem,
            "test-rows",
            "row",
            SeqNo::from_backend(1),
            &TestRow {
                value: "older".to_string(),
            },
        )
        .await
        .expect("skip older row");

        assert_eq!(
            read_test_row(&filesystem, "row").await,
            Some(TestRow {
                value: "newer".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn materialized_row_delete_skips_older_journal_sequence() {
        let filesystem = scoped_filesystem();
        put_row(
            &filesystem,
            "test-rows",
            "row",
            SeqNo::from_backend(2),
            &TestRow {
                value: "newer".to_string(),
            },
        )
        .await
        .expect("write newer row");

        delete_row(&filesystem, "test-rows", "row", SeqNo::from_backend(1))
            .await
            .expect("skip older tombstone");

        assert_eq!(
            read_test_row(&filesystem, "row").await,
            Some(TestRow {
                value: "newer".to_string(),
            })
        );

        delete_row(&filesystem, "test-rows", "row", SeqNo::from_backend(3))
            .await
            .expect("write newer tombstone");
        assert_eq!(read_test_row(&filesystem, "row").await, None);
    }

    // ─── Busy-append retry behavior (ISSUE-IRONCLAW-002) ─────────────────────

    /// Event-plane fault injection over the in-memory backend. Injected
    /// appends fail WITHOUT committing, mirroring the real
    /// [`FilesystemError::BackendBusy`] contract, so the journal's retry and
    /// halt paths can be driven deterministically.
    enum AppendInjection {
        /// The first N appends report retryable busy contention.
        BusyTimes(usize),
        /// Every append reports retryable busy contention.
        BusyForever,
        /// Every append fails with a hard backend error.
        BackendErrorForever,
    }

    struct InjectingAppendBackend {
        inner: Arc<InMemoryBackend>,
        injection: AppendInjection,
        remaining_busy: AtomicUsize,
    }

    impl InjectingAppendBackend {
        fn new(injection: AppendInjection) -> Arc<Self> {
            let remaining_busy = match &injection {
                AppendInjection::BusyTimes(count) => *count,
                _ => usize::MAX,
            };
            Arc::new(Self {
                inner: Arc::new(InMemoryBackend::new()),
                injection,
                remaining_busy: AtomicUsize::new(remaining_busy),
            })
        }

        fn injected_error(&self, path: &VirtualPath) -> Option<FilesystemError> {
            match &self.injection {
                AppendInjection::BusyTimes(_) => {
                    let current = self.remaining_busy.load(Ordering::SeqCst);
                    if current == 0 {
                        None
                    } else {
                        self.remaining_busy.store(current - 1, Ordering::SeqCst);
                        Some(FilesystemError::BackendBusy {
                            path: path.clone(),
                            operation: FilesystemOperation::Append,
                        })
                    }
                }
                AppendInjection::BusyForever => Some(FilesystemError::BackendBusy {
                    path: path.clone(),
                    operation: FilesystemOperation::Append,
                }),
                AppendInjection::BackendErrorForever => Some(FilesystemError::Backend {
                    path: path.clone(),
                    operation: FilesystemOperation::Append,
                    reason: "simulated permanent append failure".to_string(),
                }),
            }
        }
    }

    #[async_trait::async_trait]
    impl RootFilesystem for InjectingAppendBackend {
        fn capabilities(&self) -> BackendCapabilities {
            self.inner.capabilities()
        }

        async fn put(
            &self,
            path: &VirtualPath,
            entry: Entry,
            cas: CasExpectation,
        ) -> Result<RecordVersion, FilesystemError> {
            self.inner.put(path, entry, cas).await
        }

        async fn get(&self, path: &VirtualPath) -> Result<Option<VersionedEntry>, FilesystemError> {
            self.inner.get(path).await
        }

        async fn list_dir(&self, path: &VirtualPath) -> Result<Vec<DirEntry>, FilesystemError> {
            self.inner.list_dir(path).await
        }

        async fn list_dir_bounded(
            &self,
            path: &VirtualPath,
            max_entries: usize,
        ) -> Result<Vec<DirEntry>, FilesystemError> {
            self.inner.list_dir_bounded(path, max_entries).await
        }

        async fn stat(&self, path: &VirtualPath) -> Result<FileStat, FilesystemError> {
            self.inner.stat(path).await
        }

        async fn delete(&self, path: &VirtualPath) -> Result<(), FilesystemError> {
            self.inner.delete(path).await
        }

        async fn append(
            &self,
            path: &VirtualPath,
            payload: Vec<u8>,
        ) -> Result<SeqNo, FilesystemError> {
            if let Some(error) = self.injected_error(path) {
                return Err(error);
            }
            self.inner.append(path, payload).await
        }

        async fn append_batch(
            &self,
            path: &VirtualPath,
            payloads: Vec<Vec<u8>>,
        ) -> Result<Vec<SeqNo>, FilesystemError> {
            if let Some(error) = self.injected_error(path) {
                return Err(error);
            }
            self.inner.append_batch(path, payloads).await
        }

        async fn tail(
            &self,
            path: &VirtualPath,
            from: SeqNo,
        ) -> Result<Vec<EventRecord>, FilesystemError> {
            self.inner.tail(path, from).await
        }

        async fn tail_bounded(
            &self,
            path: &VirtualPath,
            from: SeqNo,
            max_records: usize,
        ) -> Result<Vec<EventRecord>, FilesystemError> {
            self.inner.tail_bounded(path, from, max_records).await
        }

        async fn reserve_sequence(&self, path: &VirtualPath) -> Result<SeqNo, FilesystemError> {
            self.inner.reserve_sequence(path).await
        }
    }

    fn non_empty_delta(tag: &str) -> SnapshotDelta {
        let mut delta = SnapshotDelta::default();
        delta.turns_delete.push(format!("turn/{tag}"));
        delta
    }

    fn journal_over(
        backend: Arc<InjectingAppendBackend>,
    ) -> (Arc<ScopedFilesystem<InjectingAppendBackend>>, DeltaJournal) {
        let mounts = MountView::new(vec![MountGrant::new(
            MountAlias::new("/turns").expect("create turns mount alias"),
            VirtualPath::new("/turns").expect("create turns virtual path"),
            MountPermissions::read_write_list_delete(),
        )])
        .expect("create turns mount view");
        let filesystem = Arc::new(ScopedFilesystem::with_fixed_view(backend, mounts));
        let journal = DeltaJournal::new(Arc::clone(&filesystem), Arc::new(AsyncMutex::new(())));
        (filesystem, journal)
    }

    async fn durable_delta_records(
        filesystem: &ScopedFilesystem<InjectingAppendBackend>,
    ) -> Vec<EventRecord> {
        filesystem
            .tail_bounded(
                &ResourceScope::system(),
                &delta_log_path().expect("delta log path"),
                SeqNo::from_backend(0),
                usize::MAX,
            )
            .await
            .expect("tail durable delta log")
    }

    #[tokio::test]
    async fn busy_append_retries_identical_batch_without_degrading() {
        // Two transient busy appends must be absorbed by retry: acks succeed,
        // the store never degrades, and the durable log carries both deltas
        // exactly once in order (busy appends never commit, so the retried
        // batch cannot duplicate or leave a gap).
        let backend = InjectingAppendBackend::new(AppendInjection::BusyTimes(2));
        let (filesystem, journal) = journal_over(Arc::clone(&backend));

        let ack = journal
            .enqueue(non_empty_delta("one"))
            .expect("enqueue first delta")
            .expect("non-empty delta yields an ack");
        DeltaJournal::await_ack(Some(ack))
            .await
            .expect("first delta acked after busy retries");
        assert!(!journal.is_degraded());

        let ack = journal
            .enqueue(non_empty_delta("two"))
            .expect("enqueue second delta")
            .expect("non-empty delta yields an ack");
        DeltaJournal::await_ack(Some(ack))
            .await
            .expect("second delta acked without contention");
        assert!(!journal.is_degraded());

        let records = durable_delta_records(filesystem.as_ref()).await;
        assert_eq!(records.len(), 2, "both deltas durable exactly once");
        let first = String::from_utf8(records[0].payload.clone()).expect("utf8 delta");
        let second = String::from_utf8(records[1].payload.clone()).expect("utf8 delta");
        assert!(first.contains("turn/one"), "first delta in order: {first}");
        assert!(
            second.contains("turn/two"),
            "second delta in order: {second}"
        );
        assert_eq!(
            backend.remaining_busy.load(Ordering::SeqCst),
            0,
            "both busy tickets consumed"
        );
        drop(journal);
    }

    #[tokio::test]
    async fn busy_append_retry_exhaustion_still_degrades_and_halts() {
        // Persistent contention beyond the retry budget keeps the
        // conservative halt: acks fail retryably and the store latches
        // degraded instead of appending behind an unknown commit state.
        let (_filesystem, journal) =
            journal_over(InjectingAppendBackend::new(AppendInjection::BusyForever));

        let ack = journal
            .enqueue(non_empty_delta("stuck"))
            .expect("enqueue delta")
            .expect("non-empty delta yields an ack");
        let result = DeltaJournal::await_ack(Some(ack)).await;
        assert!(result.is_err(), "exhausted busy retries must fail the ack");
        assert!(
            journal.is_degraded(),
            "store must latch degraded after budget"
        );
        drop(journal);
    }

    #[tokio::test]
    async fn permanent_backend_error_halts_without_retry() {
        // A hard backend error is not busy-class: no retry, immediate halt —
        // the pre-existing conservative behavior is preserved.
        let (filesystem, journal) = journal_over(InjectingAppendBackend::new(
            AppendInjection::BackendErrorForever,
        ));

        let ack = journal
            .enqueue(non_empty_delta("doomed"))
            .expect("enqueue delta")
            .expect("non-empty delta yields an ack");
        let result = DeltaJournal::await_ack(Some(ack)).await;
        assert!(
            result.is_err(),
            "permanent append failure must fail the ack"
        );
        assert!(
            journal.is_degraded(),
            "store must latch degraded immediately"
        );

        let records = durable_delta_records(filesystem.as_ref()).await;
        assert!(records.is_empty(), "nothing durable for the failed batch");
        drop(journal);
    }
}
