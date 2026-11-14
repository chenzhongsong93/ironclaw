//! Backend-proven single-statement committed current-incarnation event view.
use crate::{EventRecord, FilesystemError, FilesystemOperation};
use ironclaw_host_api::VirtualPath;
#[derive(Debug)]
pub struct CompleteEventPrefix {
    generation: String,
    head: crate::SeqNo,
    records: Vec<EventRecord>,
}
impl CompleteEventPrefix {
    pub(crate) fn generation_from_first(record: &EventRecord) -> String {
        format!(
            "{}:{}",
            record.seq.get(),
            blake3::hash(&record.payload).to_hex()
        )
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn head(&self) -> crate::SeqNo {
        self.head
    }
    pub fn into_records(self) -> Vec<EventRecord> {
        self.records
    }
    pub(crate) fn from_atomic_rows(
        path: &VirtualPath,
        expected_count: usize,
        expected_bytes: usize,
        expected_head: crate::SeqNo,
        records: Vec<EventRecord>,
    ) -> Result<Self, FilesystemError> {
        let bad = || FilesystemError::Backend {
            path: path.clone(),
            operation: FilesystemOperation::Tail,
            reason: "complete event prefix metadata mismatch".to_string(),
        };
        if records.is_empty()
            || records.len() != expected_count
            || records.last().map(|r| r.seq) != Some(expected_head)
            || records.windows(2).any(|w| w[0].seq >= w[1].seq)
            || records
                .iter()
                .try_fold(0usize, |n, r| n.checked_add(r.payload.len()))
                != Some(expected_bytes)
        {
            return Err(bad());
        }
        let first = &records[0];
        // PostgreSQL BIGSERIAL and libSQL AUTOINCREMENT event IDs are never
        // reset by normal wholepath deletion (reserved counters are separate).
        // Hash first original payload as an additional incarnation discriminator.
        let generation = Self::generation_from_first(first);
        Ok(Self {
            generation,
            head: expected_head,
            records,
        })
    }
}
