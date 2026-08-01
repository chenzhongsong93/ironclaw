//! Spawn provenance v2 hardgate callback (TianQuan debt
//! `spawn-provenance-hardgate`, cross-repo SSOT
//! `TianQuan/tests/TEST-SPEC-provenance-hardgate.md`).
//!
//! When a spawned subagent run reaches a terminal state, the settle path
//! (`await_edge::resolver::settle_and_maybe_drain`) calls
//! [`record_spawn_terminal`] to upsert one row into the shared Postgres
//! `spawn_records` table (TianQuan api migration `0005_spawn_records.sql`).
//! The TianQuan api's `run_novelist_validator` dispatch then verifies
//! `provenance.sessionId` against that table (record exists +
//! `subagent_type == "novelist"` + `terminal_status == "Completed"` +
//! `sha256(prose) == final_text_hash`), blocking LLM-written prose that
//! never went through a real novelist subagent spawn.
//!
//! Zero-cost when disabled: `TIANQUAN_SPAWN_PG_URL` unset → every call is an
//! immediate no-op (upstream IronClaw never sets it; only the TianQuan
//! docker-compose gateway service does). All errors are logged, never
//! propagated — provenance recording must never break the settle path.

use chrono::{DateTime, Utc};
use ironclaw_turns::{TurnRunId, TurnScope, TurnTimestamp};
use sha2::Digest as _;

use super::await_edge::EdgeTerminalKind;

/// One `spawn_records` row (mirror of TianQuan api migration 0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnProvenanceRecord {
    pub child_run_id: String,
    pub subagent_type: String,
    pub project_id: Option<String>,
    /// Spawn context carries no layer concept today → always `None` (never
    /// hardcode a TianQuan-specific value upstream).
    pub layer: Option<String>,
    pub spawned_at: DateTime<Utc>,
    pub terminal_status: String,
    pub final_text_hash: Option<String>,
}

impl SpawnProvenanceRecord {
    /// Assemble a record from the settle path's in-hand data.
    /// `spawned_at` is the child run's `received_at` (spawn-time proxy,
    /// backfilled at terminal time — the spawn port itself does not write).
    pub(crate) fn from_terminal(
        child_run_id: &TurnRunId,
        child_scope: &TurnScope,
        subagent_kind: &ironclaw_loop_host::SubagentKindId,
        terminal_kind: EdgeTerminalKind,
        final_text: Option<&str>,
        spawned_at: TurnTimestamp,
    ) -> Self {
        Self {
            child_run_id: child_run_id.to_string(),
            subagent_type: subagent_kind.to_string(),
            project_id: child_scope
                .project_id
                .as_ref()
                .map(|p| p.as_str().to_string()),
            layer: None,
            spawned_at,
            terminal_status: terminal_status_str(terminal_kind).to_string(),
            final_text_hash: final_text.map(sha256_hex),
        }
    }
}

/// Terminal status strings the TianQuan validator compares against
/// (`== "Completed"`); keep names exactly aligned with `EdgeTerminalKind`.
pub(crate) fn terminal_status_str(kind: EdgeTerminalKind) -> &'static str {
    match kind {
        EdgeTerminalKind::Completed => "Completed",
        EdgeTerminalKind::Failed => "Failed",
        EdgeTerminalKind::Cancelled => "Cancelled",
        EdgeTerminalKind::RecoveryRequired => "RecoveryRequired",
    }
}

/// sha256 hex lowercase (64 chars) — cross-repo contract with the TianQuan
/// api (`spawn_store::sha256_hex`); both sides hash the raw `final_text`.
pub(crate) fn sha256_hex(text: &str) -> String {
    let digest = sha2::Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Best-effort upsert into the shared `spawn_records` table.
///
/// `pg_url: None` (env `TIANQUAN_SPAWN_PG_URL` unset) → immediate no-op,
/// no network touch. Errors are warn-logged and swallowed: the settle path
/// must never fail because provenance recording did.
pub(crate) async fn record_spawn_terminal(pg_url: Option<&str>, record: &SpawnProvenanceRecord) {
    let Some(pg_url) = pg_url else {
        return;
    };
    if let Err(error) = upsert_spawn_record(pg_url, record).await {
        tracing::warn!(
            target: "tianquan_spawn_provenance",
            child_run_id = %record.child_run_id,
            error = %error,
            "spawn provenance upsert failed (best-effort, settle path unaffected)"
        );
    }
}

async fn upsert_spawn_record(
    pg_url: &str,
    record: &SpawnProvenanceRecord,
) -> Result<(), tokio_postgres::Error> {
    let (client, connection) = tokio_postgres::connect(pg_url, tokio_postgres::NoTls).await?;
    let connection_task = tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(
                target: "tianquan_spawn_provenance",
                error = %error,
                "spawn provenance pg connection driver error"
            );
        }
    });
    let result = client
        .execute(
            "INSERT INTO spawn_records \
             (child_run_id, subagent_type, project_id, layer, spawned_at, terminal_status, final_text_hash) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (child_run_id) DO UPDATE SET \
             subagent_type = EXCLUDED.subagent_type, \
             project_id = EXCLUDED.project_id, \
             layer = EXCLUDED.layer, \
             spawned_at = EXCLUDED.spawned_at, \
             terminal_status = EXCLUDED.terminal_status, \
             final_text_hash = EXCLUDED.final_text_hash",
            &[
                &record.child_run_id,
                &record.subagent_type,
                &record.project_id,
                &record.layer,
                &record.spawned_at,
                &record.terminal_status,
                &record.final_text_hash,
            ],
        )
        .await;
    drop(client);
    if let Err(error) = connection_task.await {
        tracing::warn!(
            target: "tianquan_spawn_provenance",
            error = %error,
            "spawn provenance pg connection task join error"
        );
    }
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_loop_host::SubagentKindId;

    fn kind(id: &str) -> SubagentKindId {
        SubagentKindId::new(id).expect("valid kind id")
    }

    fn scope_with_project(project_id: Option<ironclaw_host_api::ProjectId>) -> TurnScope {
        TurnScope {
            tenant_id: ironclaw_host_api::TenantId::from_trusted("tenant:t".to_string()),
            agent_id: Some(ironclaw_host_api::AgentId::from_trusted(
                "agent:a".to_string(),
            )),
            project_id,
            thread_id: ironclaw_host_api::ThreadId::from_trusted("thread:t".to_string()),
            thread_owner: Default::default(),
        }
    }

    // 覆盖:§2.2 PH-HASH-01 — 空串已知摘要(与天权侧同)
    #[test]
    fn sha256_hex_empty_known_digest() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // 覆盖:§2.2 PH-HASH-02 — 中文正文已知摘要(跨仓契约:天权 spawn_store.rs 断言同一值)
    #[test]
    fn sha256_hex_chinese_prose_known_digest() {
        assert_eq!(
            sha256_hex("夜色压城。"),
            "cc1644bbac01797d12e4c7a82bef92b4b5924bee0d694fb49dd150ce3226eaed"
        );
    }

    // 覆盖:§2.2 PH-IC-MAP-01 — terminal_kind 四变体 → status 字符串
    #[test]
    fn terminal_status_str_maps_all_variants() {
        assert_eq!(
            terminal_status_str(EdgeTerminalKind::Completed),
            "Completed"
        );
        assert_eq!(terminal_status_str(EdgeTerminalKind::Failed), "Failed");
        assert_eq!(
            terminal_status_str(EdgeTerminalKind::Cancelled),
            "Cancelled"
        );
        assert_eq!(
            terminal_status_str(EdgeTerminalKind::RecoveryRequired),
            "RecoveryRequired"
        );
    }

    // 覆盖:§2.2 PH-IC-MAP-02 — final_text=None → final_text_hash=None;Some → sha256
    #[test]
    fn record_from_terminal_hashing() {
        let child_run_id = TurnRunId::new();
        let scope = scope_with_project(None);
        let spawned_at = Utc::now();
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            None,
            spawned_at,
        );
        assert_eq!(rec.final_text_hash, None);
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some("夜色压城。"),
            spawned_at,
        );
        assert_eq!(
            rec.final_text_hash.as_deref(),
            Some("cc1644bbac01797d12e4c7a82bef92b4b5924bee0d694fb49dd150ce3226eaed")
        );
        assert_eq!(rec.child_run_id, child_run_id.to_string());
        assert_eq!(rec.subagent_type, "novelist");
        assert_eq!(rec.terminal_status, "Completed");
        assert_eq!(rec.spawned_at, spawned_at);
        assert_eq!(rec.layer, None);
    }

    // 覆盖:§2.2 PH-IC-MAP-03 — project_id 从 scope 取,None → None(不硬编码)
    #[test]
    fn record_from_terminal_project_id_passthrough() {
        let child_run_id = TurnRunId::new();
        let scope_none = scope_with_project(None);
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
            &scope_none,
            &kind("general"),
            EdgeTerminalKind::Failed,
            None,
            Utc::now(),
        );
        assert_eq!(rec.project_id, None);
        let scope_some = scope_with_project(Some(ironclaw_host_api::ProjectId::from_trusted(
            "project:iron-city".to_string(),
        )));
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
            &scope_some,
            &kind("general"),
            EdgeTerminalKind::Failed,
            None,
            Utc::now(),
        );
        assert_eq!(rec.project_id.as_deref(), Some("project:iron-city"));
    }

    // 覆盖:§2.2 PH-IC-SKIP-01 — pg_url=None(env 关闭)立即返回,零网络
    #[tokio::test]
    async fn record_spawn_terminal_none_url_is_noop() {
        let child_run_id = TurnRunId::new();
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
            &scope_with_project(None),
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some("x"),
            Utc::now(),
        );
        // 无 PG 在跑;若误触网必 panic/超时,返回即证明零开销跳过
        record_spawn_terminal(None, &rec).await;
    }
}
