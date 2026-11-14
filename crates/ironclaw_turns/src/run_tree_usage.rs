//! Generic authoritative-prefix reader. No billing, dispatch or lifecycle mutation.
use crate::events::EventCursor;
use crate::run_profile::LoopModelUsage;
use crate::{
    TurnError, TurnEventKind, TurnLifecycleEvent, TurnRunId, TurnRunRecord, TurnScope, TurnStatus,
};
use async_trait::async_trait;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub const TREE_MAX_MEMBERS: usize = 4096;
pub const TREE_MAX_DEPTH: u32 = 64;
pub const TREE_MAX_PAGE: usize = 128;
pub const TREE_MAX_PAGE_BYTES: usize = 256 * 1024;

fn denied() -> TurnError {
    TurnError::ScopeNotFound
}
fn unavailable(reason: &'static str) -> TurnError {
    TurnError::Unavailable {
        reason: reason.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunUsageProvenance {
    pub terminal_event_cursor: EventCursor,
    pub first_terminal_usage_journal_sequence: u64,
    pub terminal_event_journal_sequence: u64,
    pub usage_record_digest: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct RunTreeUsageMember {
    pub run_id: TurnRunId,
    pub parent_run_id: Option<TurnRunId>,
    pub thread_id: String,
    pub depth: u32,
    pub status: TurnStatus,
    pub usage: Option<LoopModelUsage>,
    pub usage_provenance: Option<RunUsageProvenance>,
}
#[derive(Debug, Serialize)]
pub struct RunTreeUsagePage {
    pub root_run_id: TurnRunId,
    pub snapshot_sequence: u64,
    pub members: Vec<RunTreeUsageMember>,
    pub next_cursor: Option<String>,
    pub pages_complete_at_sequence: bool,
    pub all_members_terminal_at_sequence: bool,
    pub usage_known_at_sequence: bool,
    pub permanent_closed: bool,
}

/// The normal product facade supplies its authenticated canonical-thread ACL.
/// No caller-supplied tenant/owner or same-project shortcut is accepted here.
#[async_trait]
pub trait RunTreeUsageAccess: Send + Sync {
    async fn authorize_exact_scope(&self, scope: &TurnScope) -> Result<(), TurnError>;
}
#[async_trait]
pub trait RunTreeUsagePrefixSource: Send + Sync {
    async fn current_usage_log_generation(&self) -> Result<String, TurnError>;
    async fn open_usage_tree(
        &self,
        root_scope: TurnScope,
        root_run_id: TurnRunId,
    ) -> Result<RunTreeUsageSession, TurnError>;
}

// No Deserialize/public constructor: only the authoritative prefix decoder
// can mint this session. Keep this original object across all pages.
pub struct RunTreeUsageSession {
    root_scope: TurnScope,
    root: TurnRunId,
    sequence: u64,
    generation: String,
    nonce: String,
    rows: Vec<(TurnScope, RunTreeUsageMember)>,
    next: usize,
    expected_cursor: Option<String>,
    finished: bool,
}
impl RunTreeUsageSession {
    pub fn log_generation(&self) -> &str {
        &self.generation
    }
    pub fn root_scope(&self) -> &TurnScope {
        &self.root_scope
    }
    pub fn root_run_id(&self) -> TurnRunId {
        self.root
    }
    pub async fn read_page(
        &mut self,
        access: &dyn RunTreeUsageAccess,
        original_root_scope: &TurnScope,
        supplied_cursor: Option<&str>,
        limit: usize,
    ) -> Result<RunTreeUsagePage, TurnError> {
        if self.finished
            || original_root_scope != &self.root_scope
            || supplied_cursor != self.expected_cursor.as_deref()
            || !(1..=TREE_MAX_PAGE).contains(&limit)
        {
            return Err(denied());
        }
        // Root and every member are reauthorized on EVERY page, including
        // historical members on other threads; revocation fails the entire page.
        access.authorize_exact_scope(&self.root_scope).await?;
        for (scope, _) in &self.rows {
            access.authorize_exact_scope(scope).await?;
        }
        let end = self.next.saturating_add(limit).min(self.rows.len());
        let members = self.rows[self.next..end]
            .iter()
            .map(|(_, m)| m.clone())
            .collect();
        let next_cursor = (end < self.rows.len())
            .then(|| format!("{}:{}:{}:{}", self.nonce, self.root, self.sequence, end));
        let page = RunTreeUsagePage {
            root_run_id: self.root,
            snapshot_sequence: self.sequence,
            members,
            next_cursor: next_cursor.clone(),
            pages_complete_at_sequence: end == self.rows.len(),
            all_members_terminal_at_sequence: self.rows.iter().all(|(_, m)| m.status.is_terminal()),
            usage_known_at_sequence: self
                .rows
                .iter()
                .all(|(_, m)| m.usage.is_some() && m.usage_provenance.is_some()),
            permanent_closed: false,
        };
        if serde_json::to_vec(&page)
            .map_err(|_| unavailable("usage page serialization failed"))?
            .len()
            > TREE_MAX_PAGE_BYTES
        {
            return Err(unavailable("usage page byte budget exceeded"));
        }
        // Advance only after all ACLs and final byte bound succeed.
        self.next = end;
        self.expected_cursor = next_cursor;
        self.finished = page.pages_complete_at_sequence;
        Ok(page)
    }
}

pub(crate) struct RunUsagePrefix {
    last_sequence: u64,
    rows: BTreeMap<TurnRunId, TurnRunRecord>,
    provenance: BTreeMap<TurnRunId, RunUsageProvenance>,
    births: BTreeSet<TurnRunId>,
    terminal_events: BTreeMap<TurnRunId, BTreeMap<u64, (u64, TurnLifecycleEvent)>>,
}
impl RunUsagePrefix {
    pub(crate) fn new() -> Self {
        Self {
            last_sequence: 0,
            rows: BTreeMap::new(),
            provenance: BTreeMap::new(),
            births: BTreeSet::new(),
            terminal_events: BTreeMap::new(),
        }
    }
    pub(crate) fn ingest(
        &mut self,
        sequence: u64,
        rows: Vec<TurnRunRecord>,
        events: &[TurnLifecycleEvent],
        has_run_deletes: bool,
    ) -> Result<(), TurnError> {
        if sequence <= self.last_sequence || has_run_deletes {
            return Err(unavailable("usage prefix gap or unsupported run tombstone"));
        }
        // Witness comes from the original durable lifecycle event, never from
        // status/model_usage fields on a run upsert alone. Preserve all token
        // dimensions for Completed, Failed and Cancelled equally.
        for event in events {
            let matched = matches!(
                (&event.kind, event.status),
                (TurnEventKind::Completed, TurnStatus::Completed)
                    | (TurnEventKind::Failed, TurnStatus::Failed)
                    | (TurnEventKind::Cancelled, TurnStatus::Cancelled)
            );
            if !matched {
                continue;
            }
            let by_cursor = self.terminal_events.entry(event.run_id).or_default();
            if let Some((_, prior)) = by_cursor.get(&event.cursor.0) {
                if prior != event {
                    return Err(unavailable("terminal lifecycle event changed"));
                }
            } else {
                by_cursor.insert(event.cursor.0, (sequence, event.clone()));
            }
        }
        let mut seen = BTreeSet::new();
        for r in rows {
            if !seen.insert(r.run_id) {
                return Err(unavailable("duplicate run within journal delta"));
            }
            if !self.rows.contains_key(&r.run_id)
                && r.status == TurnStatus::Queued
                && r.claim_count == 0
                && r.runner_id.is_none()
                && r.lease_token.is_none()
                && events.iter().any(|e| {
                    e.run_id == r.run_id
                        && e.scope == r.scope
                        && e.status == TurnStatus::Queued
                        && e.cursor == r.event_cursor
                        && matches!(e.kind, TurnEventKind::Submitted | TurnEventKind::Resumed)
                })
            {
                self.births.insert(r.run_id);
            }
            if let Some(old) = self.rows.get(&r.run_id) {
                if old.scope != r.scope
                    || old.parent_run_id != r.parent_run_id
                    || old.subagent_depth != r.subagent_depth
                    || old.spawn_tree_root_run_id != r.spawn_tree_root_run_id
                {
                    return Err(unavailable("run lineage changed in durable prefix"));
                }
                if self.provenance.contains_key(&r.run_id)
                    && (old.model_usage != r.model_usage
                        || old.event_cursor != r.event_cursor
                        || old.status != r.status
                        || !r.status.is_terminal())
                {
                    return Err(unavailable("terminal usage changed or reopened same run"));
                }
            }
            if r.status.is_terminal()
                && r.model_usage
                    .is_some_and(|usage| usage.normalized_input_total().is_some())
                && !self.provenance.contains_key(&r.run_id)
            {
                let (event_sequence, event) = self
                    .terminal_events
                    .get(&r.run_id)
                    .and_then(|by_cursor| by_cursor.get(&r.event_cursor.0))
                    .filter(|(_, event)| {
                        event.scope == r.scope
                            && event.status == r.status
                            && event.model_usage == r.model_usage
                    })
                    .ok_or_else(|| unavailable("matching original terminal usage event missing"))?;
                let body = serde_json::to_vec(&(
                    r.run_id,
                    &r.scope,
                    r.parent_run_id,
                    r.spawn_tree_root_run_id,
                    r.subagent_depth,
                    r.event_cursor,
                    r.model_usage,
                    event_sequence,
                    &event.kind,
                    event.status,
                    event.cursor,
                    event.model_usage,
                ))
                .map_err(|_| unavailable("usage provenance serialization failed"))?;
                self.provenance.insert(
                    r.run_id,
                    RunUsageProvenance {
                        terminal_event_cursor: r.event_cursor,
                        first_terminal_usage_journal_sequence: sequence,
                        terminal_event_journal_sequence: *event_sequence,
                        usage_record_digest: blake3::hash(&body).to_hex().to_string(),
                    },
                );
            }
            self.rows.insert(r.run_id, r);
            if self.rows.len() > 65536 {
                return Err(unavailable("usage prefix retained-run budget exceeded"));
            }
        }
        self.last_sequence = sequence;
        Ok(())
    }
    pub(crate) fn contains_run(&self, id: &TurnRunId) -> bool {
        self.rows.contains_key(id)
    }
    pub(crate) fn finish(
        self,
        root_scope: TurnScope,
        root: TurnRunId,
        sequence: u64,
        generation: String,
    ) -> Result<RunTreeUsageSession, TurnError> {
        if generation.is_empty() || sequence == 0 || self.last_sequence != sequence {
            return Err(unavailable("usage prefix incomplete"));
        }
        let r = self
            .rows
            .get(&root)
            .filter(|r| r.scope == root_scope)
            .ok_or_else(denied)?;
        if r.parent_run_id.is_some()
            || r.subagent_depth != 0
            || r.spawn_tree_root_run_id.is_some_and(|x| x != root)
        {
            return Err(denied());
        }
        let mut members = BTreeSet::from([root]);
        // Include EVERY retained row naming this root, not only reachable rows:
        // this makes orphaned/foreign members an error instead of hiding them.
        for r in self.rows.values() {
            if r.spawn_tree_root_run_id == Some(root) {
                members.insert(r.run_id);
            }
        }
        loop {
            let before = members.len();
            for r in self.rows.values() {
                if r.parent_run_id.is_some_and(|p| members.contains(&p)) {
                    members.insert(r.run_id);
                }
            }
            if members.len() > TREE_MAX_MEMBERS {
                return Err(unavailable("usage tree member budget exceeded"));
            }
            if members.len() == before {
                break;
            }
        }
        if members.len() > TREE_MAX_MEMBERS {
            return Err(unavailable("usage tree member budget exceeded"));
        }
        let mut rows = Vec::new();
        for id in &members {
            if !self.births.contains(id) {
                return Err(unavailable(
                    "original run birth absent from current log incarnation",
                ));
            }
            let r = self
                .rows
                .get(id)
                .ok_or_else(|| unavailable("usage member missing"))?;
            if r.scope.tenant_id != root_scope.tenant_id
                || r.scope.agent_id != root_scope.agent_id
                || r.scope.project_id != root_scope.project_id
                || r.subagent_depth > TREE_MAX_DEPTH
            {
                return Err(denied());
            }
            if *id != root && r.spawn_tree_root_run_id != Some(root) {
                return Err(unavailable("child missing or mismatched canonical root"));
            }
            let mut path = BTreeSet::new();
            let mut cursor = *id;
            while cursor != root {
                if !path.insert(cursor) {
                    return Err(unavailable("usage tree cycle"));
                }
                let node = self
                    .rows
                    .get(&cursor)
                    .ok_or_else(|| unavailable("usage ancestor missing"))?;
                let parent = node
                    .parent_run_id
                    .ok_or_else(|| unavailable("usage tree orphan"))?;
                if !members.contains(&parent) {
                    return Err(unavailable("usage parent outside root"));
                }
                let p = self
                    .rows
                    .get(&parent)
                    .ok_or_else(|| unavailable("usage parent missing"))?;
                if p.subagent_depth.checked_add(1) != Some(node.subagent_depth) {
                    return Err(unavailable("usage depth mismatch"));
                }
                cursor = parent;
            }
            rows.push((
                r.scope.clone(),
                RunTreeUsageMember {
                    run_id: r.run_id,
                    parent_run_id: r.parent_run_id,
                    thread_id: r.scope.thread_id.as_str().to_string(),
                    depth: r.subagent_depth,
                    status: r.status,
                    usage: r.model_usage,
                    usage_provenance: self.provenance.get(id).cloned(),
                },
            ));
        }
        Ok(RunTreeUsageSession {
            root_scope,
            root,
            sequence,
            generation,
            nonce: uuid::Uuid::new_v4().to_string(),
            rows,
            next: 0,
            expected_cursor: None,
            finished: false,
        })
    }
}

#[cfg(test)]
mod tests;
