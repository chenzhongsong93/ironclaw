//! Spawn provenance v2 hardgate callback (TianQuan debt
//! `spawn-provenance-hardgate`, cross-repo SSOT
//! `TianQuan/tests/TEST-SPEC-provenance-hardgate.md`).
//!
//! After `submit_child_run` returns Accepted, the await-edge writer inserts a
//! provisional `Spawned` row into the shared Postgres `spawn_records` table.
//! When the child reaches a terminal state, the settle path
//! (`await_edge::resolver::settle_and_maybe_drain`) calls
//! [`record_spawn_terminal`] to update that same row (TianQuan api migration
//! `0005_spawn_records.sql`).
//! The TianQuan api's `run_novelist_validator` dispatch then verifies
//! `provenance.sessionId` against that table (record exists +
//! `subagent_type == "novelist"` + `terminal_status == "Completed"` +
//! `sha256(prose) == final_text_hash`), blocking LLM-written prose that
//! never went through a real novelist subagent spawn.
//!
//! 铁律(2026-08-07):通用平台能力不做领域判断——本扩展只做**忠实文件持久化**
//! (raw final_text 落盘 + raw sha256),零小说领域假设(剥围栏/判正文/拒汇报等
//! 全属天权业务逻辑,在天权 api 读取侧 `read_prose_from_ssot_file` 实现)。
//! raw 落盘 = 完整过程记录(可 debug);天权读取时提取纯正文用于呈现/校验。
//!
//! Zero-cost when disabled: `TIANQUAN_SPAWN_PG_URL` unset → every call is an
//! immediate no-op (upstream IronClaw never sets it; only the TianQuan
//! docker-compose gateway service does). All errors are logged, never
//! propagated — provenance recording must never break the settle path.

use chrono::{DateTime, Utc};
use ironclaw_turns::{TurnRunId, TurnScope, TurnTimestamp};
use sha2::Digest as _;
use std::io::Write as _;

#[cfg(test)]
pub(super) static TEST_PG_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use super::await_edge::EdgeTerminalKind;

// 铁律(2026-08-07):通用平台能力不做领域判断——本扩展只做**忠实文件持久化**
// (子 agent final_text 原文落盘 + raw sha256),零小说领域假设(剥围栏/判正文/拒汇报
// 等全属天权业务逻辑,在天权 api 读取侧 `read_prose_from_ssot_file` 实现)。
// 落盘 raw = 完整过程记录(可 debug),天权读取时提取纯正文用于呈现/校验。

/// One `spawn_records` row (mirror of TianQuan api migration 0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnProvenanceRecord {
    pub child_run_id: String,
    /// Trusted parent from the persisted child-run relationship, never a model-supplied value.
    pub parent_run_id: String,
    pub subagent_type: String,
    pub project_id: Option<String>,
    /// Spawn context carries no layer concept today → always `None` (never
    /// hardcode a TianQuan-specific value upstream).
    pub layer: Option<String>,
    pub spawned_at: DateTime<Utc>,
    pub terminal_status: String,
    /// raw final_text 的 sha256(通用契约:天权 validator 用 LLM 传的 prose 原文比对此值,
    /// 证明 prose 真来自 spawn;领域提取不参与此 hash)。
    pub final_text_hash: Option<String>,
    /// sanitized terminal reason(失败类别,如 iteration_limit / driver_protocol_violation)。
    /// 通用契约:settle 侧 event.sanitized_reason 忠实落库,零领域判断;天权侧按它归类
    /// 失败 spawn(ISSUE-IRONCLAW-009 留观项:child loop-exit 空转的失败归类)。
    /// Completed 恒 None。
    pub failure_category: Option<String>,
}

pub(crate) struct SpawnTerminalInput<'a> {
    pub child_run_id: &'a TurnRunId,
    pub parent_run_id: &'a TurnRunId,
    pub child_scope: &'a TurnScope,
    pub subagent_kind: &'a ironclaw_loop_host::SubagentKindId,
    pub terminal_kind: EdgeTerminalKind,
    pub final_text: Option<&'a str>,
    pub spawned_at: TurnTimestamp,
    pub failure_category: Option<String>,
}

impl SpawnProvenanceRecord {
    /// Assemble the provisional provenance row after the awaited-child edge is durable.
    pub(crate) fn from_spawned(
        child_run_id: &TurnRunId,
        parent_run_id: &TurnRunId,
        child_scope: &TurnScope,
        subagent_kind: &ironclaw_loop_host::SubagentKindId,
        spawned_at: DateTime<Utc>,
    ) -> Self {
        Self {
            child_run_id: child_run_id.to_string(),
            parent_run_id: parent_run_id.to_string(),
            subagent_type: subagent_kind.to_string(),
            project_id: child_scope
                .project_id
                .as_ref()
                .map(|project| project.as_str().to_string()),
            // Generic host scope has no layer concept. Keep it unknown; TianQuan
            // rejects layer-bound scope refs until the trusted label is propagated.
            layer: None,
            spawned_at,
            terminal_status: "Spawned".to_string(),
            final_text_hash: None,
            failure_category: None,
        }
    }

    /// Assemble a terminal update from the settle path's in-hand data.
    /// If no start row exists (for example, an older run or exporter outage),
    /// the child's `received_at` remains a compatibility timestamp fallback.
    pub(crate) fn from_terminal(input: SpawnTerminalInput<'_>) -> Self {
        let SpawnTerminalInput {
            child_run_id,
            parent_run_id,
            child_scope,
            subagent_kind,
            terminal_kind,
            final_text,
            spawned_at,
            failure_category,
        } = input;
        Self {
            child_run_id: child_run_id.to_string(),
            parent_run_id: parent_run_id.to_string(),
            subagent_type: subagent_kind.to_string(),
            project_id: child_scope
                .project_id
                .as_ref()
                .map(|p| p.as_str().to_string()),
            layer: None,
            spawned_at,
            terminal_status: terminal_status_str(terminal_kind).to_string(),
            // 铁律(2026-08-07):通用平台只做忠实持久化——final_text_hash = raw final_text 的 sha256
            // (不剥围栏/不做领域判断)。领域提取(剥围栏/拒汇报/判正文)在天权 api 读取侧实现;
            // 天权 validator 用 LLM 传的 prose 原文比对此值,证明 prose 真来自 spawn。
            final_text_hash: final_text.map(sha256_hex),
            failure_category: match terminal_kind {
                // Completed 无失败类别;其余终态忠实记录(调用方已 sanitize,不再加工)。
                EdgeTerminalKind::Completed => None,
                _ => failure_category.filter(|reason| !reason.trim().is_empty()),
            },
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
/// api (`spawn_store::sha256_hex`); both sides hash the raw `final_text`。
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
///
/// 2026-08-04:正文落盘标准(天权用户拍板)——settle 时由引擎直接落盘(非 LLM 转抄),
/// api validator 从文件读 prose 做 hash 比对,主 agent 不转抄长文。
/// 2026-08-07 治本:落盘/哈希只用提取正文(剥 ```json``` 围栏),见 extract_prose_from_final_text。
/// env TIANQUAN_SPAWN_PROSE_DIR 控制目录(未设跳过)。
pub(crate) async fn record_spawn_terminal(
    pg_url: Option<&str>,
    record: &SpawnProvenanceRecord,
    final_text: Option<&str>,
) {
    if !persist_final_text_to_workspace(final_text, record) {
        // 已有同 child 的不同原文或落盘失败：不更新 PG hash，保留首次可信来源。
        return;
    }
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

/// Best-effort insert of the provisional child identity once its await edge is durable.
///
/// The insert is idempotent and never overwrites an existing terminal row. Errors are
/// logged and swallowed, matching the terminal exporter so provenance cannot break spawn.
pub(crate) async fn record_spawn_started(pg_url: Option<&str>, record: &SpawnProvenanceRecord) {
    let Some(pg_url) = pg_url else {
        return;
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        insert_spawn_started(pg_url, record),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            target: "tianquan_spawn_provenance",
            child_run_id = %record.child_run_id,
            error = %error,
            "spawn start provenance insert failed (best-effort, spawn path unaffected)"
        ),
        Err(_) => tracing::warn!(
            target: "tianquan_spawn_provenance",
            child_run_id = %record.child_run_id,
            "spawn start provenance insert timed out after 2 seconds (best-effort)"
        ),
    }
}

/// 把子 agent final_text 原文忠实落盘到 workspace 正文文件(引擎侧落盘,零 LLM 转抄)。
///
/// 铁律(2026-08-07):通用平台只做忠实持久化——落盘 raw final_text 原文,不做领域判断
/// (剥围栏/判正文/拒汇报全属天权业务逻辑,在天权 api 读取侧 `read_prose_from_ssot_file`
/// 提取纯正文用于呈现/校验)。raw 落盘 = 完整过程记录(可 debug)。
/// 路径:{TIANQUAN_SPAWN_PROSE_DIR}/runs/{child_run_id}.txt。平台只保存 run 原文，
/// 不猜领域项目/章号；同 child 重放必须字节相同。env 未设时跳过文件写。
fn persist_final_text_to_workspace(
    final_text: Option<&str>,
    record: &SpawnProvenanceRecord,
) -> bool {
    let Some(prose_dir) = std::env::var("TIANQUAN_SPAWN_PROSE_DIR").ok() else {
        return true;
    };
    let Some(prose) = final_text else {
        return true;
    };
    let dir = std::path::Path::new(&prose_dir).join("runs");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            target: "tianquan_spawn_provenance",
            child_run_id = %record.child_run_id,
            dir = %dir.display(),
            error = %error,
            "spawn prose dir create failed (best-effort, settle path unaffected)"
        );
        return false;
    }
    let path = dir.join(format!("{}.txt", record.child_run_id));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(prose.as_bytes()) {
                tracing::warn!(
                    target: "tianquan_spawn_provenance",
                    child_run_id = %record.child_run_id,
                    error = %error,
                    "spawn raw write failed; provenance row not updated"
                );
                return false;
            }
            tracing::info!(
                target: "tianquan_spawn_provenance",
                child_run_id = %record.child_run_id,
                path = %path.display(),
                byte_len = prose.len(),
                "spawn raw persisted by child run"
            );
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::read_to_string(&path) {
                Ok(existing) if existing == prose => true,
                Ok(_) | Err(_) => {
                    tracing::warn!(
                        target: "tianquan_spawn_provenance",
                        child_run_id = %record.child_run_id,
                        "different or unreadable raw already exists for child; provenance row not updated"
                    );
                    false
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                target: "tianquan_spawn_provenance",
                child_run_id = %record.child_run_id,
                error = %error,
                "spawn raw open failed; provenance row not updated"
            );
            false
        }
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
            // Terminal callbacks complete provenance but may not erase the
            // identity captured at accepted submission.
            "INSERT INTO spawn_records \
             (child_run_id, parent_run_id, subagent_type, project_id, layer, spawned_at, terminal_status, final_text_hash, failure_category) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
            ON CONFLICT (child_run_id) DO UPDATE SET \
             parent_run_id = COALESCE(spawn_records.parent_run_id, EXCLUDED.parent_run_id), \
             subagent_type = spawn_records.subagent_type, \
             project_id = COALESCE(spawn_records.project_id, EXCLUDED.project_id), \
             layer = COALESCE(spawn_records.layer, EXCLUDED.layer), \
             spawned_at = spawn_records.spawned_at, \
             terminal_status = EXCLUDED.terminal_status, \
             final_text_hash = EXCLUDED.final_text_hash, \
             failure_category = EXCLUDED.failure_category",
            &[
                &record.child_run_id,
                &record.parent_run_id,
                &record.subagent_type,
                &record.project_id,
                &record.layer,
                &record.spawned_at,
                &record.terminal_status,
                &record.final_text_hash,
                &record.failure_category,
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

async fn insert_spawn_started(
    pg_url: &str,
    record: &SpawnProvenanceRecord,
) -> Result<(), tokio_postgres::Error> {
    let (client, connection) = tokio_postgres::connect(pg_url, tokio_postgres::NoTls).await?;
    let connection_task = tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(
                target: "tianquan_spawn_provenance",
                error = %error,
                "spawn-start provenance pg connection driver error"
            );
        }
    });
    let result = client
        .execute(
            "INSERT INTO spawn_records \
             (child_run_id,parent_run_id,subagent_type,project_id,layer,spawned_at,terminal_status,final_text_hash,failure_category) \
             VALUES ($1,$2,$3,$4,$5,$6,'Spawned',NULL,NULL) \
             ON CONFLICT (child_run_id) DO NOTHING",
            &[
                &record.child_run_id,
                &record.parent_run_id,
                &record.subagent_type,
                &record.project_id,
                &record.layer,
                &record.spawned_at,
            ],
        )
        .await;
    drop(client);
    if let Err(error) = connection_task.await {
        tracing::warn!(
            target: "tianquan_spawn_provenance",
            error = %error,
            "spawn-start provenance pg connection task join error"
        );
    }
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_loop_host::SubagentKindId;

    macro_rules! terminal_record {
        ($child:expr, $parent:expr, $scope:expr, $kind:expr,
         $terminal:expr, $text:expr, $spawned_at:expr, $failure:expr $(,)?) => {
            SpawnProvenanceRecord::from_terminal(SpawnTerminalInput {
                child_run_id: $child,
                parent_run_id: $parent,
                child_scope: $scope,
                subagent_kind: $kind,
                terminal_kind: $terminal,
                final_text: $text,
                spawned_at: $spawned_at,
                failure_category: $failure,
            })
        };
    }

    static PROSE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    // 覆盖:§2.2 PH-IC-MAP-02 — final_text=None → final_text_hash=None;
    // Some → raw final_text 的 sha256(铁律:忠实持久化,不剥围栏不做领域判断)
    #[test]
    fn record_from_terminal_hashing() {
        let child_run_id = TurnRunId::new();
        let scope = scope_with_project(None);
        let spawned_at = Utc::now();
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            None,
            spawned_at,
            None,
        );
        assert_eq!(rec.final_text_hash, None);
        // 忠实持久化:任意 final_text(含围栏/汇报)hash = raw 的 sha256,不做领域判断
        let direct_prose = "夜色压城。".repeat(30); // ≥100 字
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some(&direct_prose),
            spawned_at,
            None,
        );
        assert_eq!(
            rec.final_text_hash.as_deref(),
            Some(sha256_hex(&direct_prose).as_str()),
            "hash = raw final_text 的 sha256(忠实持久化,不剥围栏)"
        );
        // 围栏形态同样 = raw 全文 sha256(领域提取在天权 api 侧)
        let prose = "夜色压城。".repeat(30); // >100 字
        let final_text = format!(
            "第24章 开头\n\n{prose}```json\n{{\"prose\": \"{prose}\", \"usedGraphFacts\": [\"F1\"]}}\n```"
        );
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some(&final_text),
            spawned_at,
            None,
        );
        assert_eq!(
            rec.final_text_hash.as_deref(),
            Some(sha256_hex(&final_text).as_str()),
            "围栏形态 hash = raw 全文 sha256(通用平台不剥围栏)"
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
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope_none,
            &kind("general"),
            EdgeTerminalKind::Failed,
            None,
            Utc::now(),
            None,
        );
        assert_eq!(rec.project_id, None);
        let scope_some = scope_with_project(Some(ironclaw_host_api::ProjectId::from_trusted(
            "project:iron-city".to_string(),
        )));
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope_some,
            &kind("general"),
            EdgeTerminalKind::Failed,
            None,
            Utc::now(),
            None,
        );
        assert_eq!(rec.project_id.as_deref(), Some("project:iron-city"));
    }

    #[test]
    fn terminal_record_keeps_trusted_parent_run_id() {
        // 来源绑定以子记录上可信父 run 为准，不能从模型任务文本猜项目。
        let parent_run_id = TurnRunId::new();
        let child_run_id = TurnRunId::new();
        let record = terminal_record!(
            &child_run_id,
            &parent_run_id,
            &scope_with_project(None),
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some("正文"),
            Utc::now(),
            None,
        );
        assert_eq!(record.parent_run_id, parent_run_id.to_string());
    }

    // 覆盖:§2.2 PH-IC-SKIP-01 — pg_url=None(env 关闭)立即返回,零网络
    #[tokio::test]
    async fn record_spawn_terminal_none_url_is_noop() {
        let child_run_id = TurnRunId::new();
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope_with_project(None),
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some("x"),
            Utc::now(),
            None,
        );
        // 无 PG 在跑;若误触网必 panic/超时,返回即证明零开销跳过
        record_spawn_terminal(None, &rec, None).await;
    }

    // 覆盖:2026-08-04 落盘标准 + 2026-08-07 铁律——raw final_text 忠实落盘(通用平台不做领域判断)
    #[test]
    fn persist_final_text_uses_child_run_address_instead_of_fixed_chapter() {
        let _guard = PROSE_ENV_LOCK.lock().expect("serialize prose env tests");
        // 用一次性临时目录避免污染真实 workspace
        let tmp = std::env::temp_dir().join(format!("tq-spawn-prose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe {
            std::env::set_var("TIANQUAN_SPAWN_PROSE_DIR", &tmp);
        }
        let child_run_id = TurnRunId::new();
        // project_id=None 走 fallback "iron-city"(子 agent scope 不带 project,2026-08-04)
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope_with_project(None),
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some("x"),
            Utc::now(),
            None,
        );
        // 忠实持久化:任意 final_text(含汇报/围栏)→ 原样落盘(通用平台不判断内容)
        let direct_prose = "夜".repeat(200);
        persist_final_text_to_workspace(Some(&direct_prose), &rec);
        let direct_path = tmp.join("runs").join(format!("{child_run_id}.txt"));
        assert!(direct_path.exists(), "正文直出应落盘");
        assert!(
            !tmp.join("iron-city")
                .join("chapters")
                .join("ch24.txt")
                .exists(),
            "平台原文不能覆盖固定项目/章节文件"
        );
        let written_direct = std::fs::read_to_string(&direct_path).expect("读回");
        assert_eq!(
            written_direct, direct_prose,
            "忠实落盘:无围栏 final_text = 原文"
        );

        // 汇报文本同样忠实落盘(raw 过程记录,可 debug;领域拒汇报在天权 api 读取侧)
        let report = format!(
            "当前状态汇报: spawn 被拒 fanout 超限。{}\n<suggestions>[\"重试\"]</suggestions>",
            "等待".repeat(30)
        );
        let report_child = TurnRunId::new();
        let report_record = terminal_record!(
            &report_child,
            &TurnRunId::new(),
            &scope_with_project(None),
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some(&report),
            Utc::now(),
            None,
        );
        persist_final_text_to_workspace(Some(&report), &report_record);
        let report_path = tmp.join("runs").join(format!("{report_child}.txt"));
        let written_report = std::fs::read_to_string(&report_path).expect("读回");
        assert_eq!(
            written_report, report,
            "忠实落盘:汇报文本也原样落盘(领域判断不在通用平台)"
        );

        // 围栏形态同样忠实落盘 = raw 全文(剥围栏在天权 api 读取侧)
        let prose = "夜".repeat(200);
        let fenced =
            format!("```json\n{{\"prose\": \"{prose}\", \"usedGraphFacts\": [\"F1\"]}}\n```");
        let fenced_child = TurnRunId::new();
        let fenced_record = terminal_record!(
            &fenced_child,
            &TurnRunId::new(),
            &scope_with_project(None),
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some(&fenced),
            Utc::now(),
            None,
        );
        persist_final_text_to_workspace(Some(&fenced), &fenced_record);
        let fenced_path = tmp.join("runs").join(format!("{fenced_child}.txt"));
        let written_fenced = std::fs::read_to_string(&fenced_path).expect("读回");
        assert_eq!(
            written_fenced, fenced,
            "忠实落盘:围栏形态 = raw 全文(不剥围栏)"
        );
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe {
            std::env::remove_var("TIANQUAN_SPAWN_PROSE_DIR");
        }
    }

    #[test]
    fn a_replayed_child_cannot_replace_its_original_raw_text() {
        let _guard = PROSE_ENV_LOCK.lock().expect("serialize prose env tests");
        let temp = tempfile::tempdir().expect("isolated raw directory");
        unsafe { std::env::set_var("TIANQUAN_SPAWN_PROSE_DIR", temp.path()) };
        let child_run_id = TurnRunId::new();
        let record = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope_with_project(None),
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some("first"),
            Utc::now(),
            None,
        );
        persist_final_text_to_workspace(Some("first"), &record);
        persist_final_text_to_workspace(Some("different"), &record);
        unsafe { std::env::remove_var("TIANQUAN_SPAWN_PROSE_DIR") };
        let run_path = temp.path().join("runs").join(format!("{child_run_id}.txt"));
        let old_path = temp.path().join("iron-city/chapters/ch24.txt");
        let persisted = if run_path.exists() {
            run_path
        } else {
            old_path
        };
        assert_eq!(
            std::fs::read_to_string(persisted).expect("raw text should exist"),
            "first"
        );
    }

    // 覆盖:ISSUE-IRONCLAW-009 留观项收口条件①——失败终态的 sanitized reason 忠实落库
    // (failure_category),Completed 恒 None;天权侧按它归类失败 spawn,不再依赖已丢的网关日志。
    #[test]
    fn record_from_terminal_captures_failure_category() {
        let child_run_id = TurnRunId::new();
        let scope = scope_with_project(None);
        // Failed + 非空 reason → 记录
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Failed,
            None,
            Utc::now(),
            Some("driver_protocol_violation".to_string()),
        );
        assert_eq!(
            rec.failure_category.as_deref(),
            Some("driver_protocol_violation"),
            "Failed 终态必须忠实落库 sanitized reason"
        );
        // 空白 reason → None(不落噪声)
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Failed,
            None,
            Utc::now(),
            Some("   ".to_string()),
        );
        assert_eq!(rec.failure_category, None);
        // Completed → 恒 None(即便传入也不记录,Completed 无失败类别)
        let rec = terminal_record!(
            &child_run_id,
            &TurnRunId::new(),
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some("x"),
            Utc::now(),
            Some("iteration_limit".to_string()),
        );
        assert_eq!(rec.failure_category, None);
        // Cancelled / RecoveryRequired → 同样记录
        for kind_ in [
            EdgeTerminalKind::Cancelled,
            EdgeTerminalKind::RecoveryRequired,
        ] {
            let rec = terminal_record!(
                &child_run_id,
                &TurnRunId::new(),
                &scope,
                &kind("novelist"),
                kind_,
                None,
                Utc::now(),
                Some("recovery_required".to_string()),
            );
            assert_eq!(rec.failure_category.as_deref(), Some("recovery_required"));
        }
    }

    #[tokio::test]
    #[ignore = "requires disposable atomic_spawn_test PostgreSQL"]
    async fn postgres_export_keeps_parent_run_id_and_raw_hash() {
        let _env_guard = super::TEST_PG_ENV_LOCK.lock().await;
        let pg_url = std::env::var("TIANQUAN_ATOMIC_TEST_PG_URL")
            .expect("disposable PG URL must be explicit");
        assert!(
            pg_url.starts_with("postgres://tq_atomic_test:")
                && pg_url.ends_with("/atomic_spawn_test"),
            "the exporter test must not access a business database"
        );
        let (client, connection) = tokio_postgres::connect(&pg_url, tokio_postgres::NoTls)
            .await
            .expect("connect disposable PG");
        let driver = tokio::spawn(async move { connection.await.expect("PG connection driver") });
        client
            .batch_execute(
                "CREATE TABLE spawn_records (\
                 child_run_id TEXT PRIMARY KEY, parent_run_id TEXT, subagent_type TEXT NOT NULL, \
                 project_id TEXT, layer TEXT, spawned_at TIMESTAMPTZ NOT NULL, \
                 terminal_status TEXT, final_text_hash TEXT, failure_category TEXT)",
            )
            .await
            .expect("create disposable spawn table");
        let parent = TurnRunId::new();
        let child = TurnRunId::new();
        let raw = "原文不允许被重写。";
        let scope = scope_with_project(Some(ironclaw_host_api::ProjectId::from_trusted(
            "atomic-spawn".to_string(),
        )));
        let spawned_at = Utc::now();
        let started = SpawnProvenanceRecord::from_spawned(
            &child,
            &parent,
            &scope,
            &kind("novelist"),
            spawned_at,
        );
        record_spawn_started(Some(&pg_url), &started).await;
        record_spawn_started(Some(&pg_url), &started).await;
        let started_row = client
            .query_one(
                "SELECT parent_run_id, subagent_type, project_id, terminal_status, \
                 final_text_hash, spawned_at FROM spawn_records WHERE child_run_id=$1",
                &[&started.child_run_id],
            )
            .await
            .expect("read back idempotent spawn-start record");
        assert_eq!(
            started_row.get::<_, Option<String>>(0),
            Some(parent.to_string())
        );
        assert_eq!(started_row.get::<_, String>(1), "novelist");
        assert_eq!(
            started_row.get::<_, Option<String>>(2),
            Some("atomic-spawn".into())
        );
        assert_eq!(started_row.get::<_, String>(3), "Spawned");
        assert_eq!(started_row.get::<_, Option<String>>(4), None);
        let persisted_spawned_at: DateTime<Utc> = started_row.get(5);
        let record = terminal_record!(
            &child,
            &parent,
            &scope_with_project(None),
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some(raw),
            spawned_at,
            None,
        );
        record_spawn_terminal(Some(&pg_url), &record, None).await;
        let row = client
            .query_one(
                "SELECT parent_run_id, project_id, final_text_hash, terminal_status, spawned_at \
                 FROM spawn_records WHERE child_run_id=$1",
                &[&record.child_run_id],
            )
            .await
            .expect("read back exact terminal record");
        assert_eq!(row.get::<_, Option<String>>(0), Some(parent.to_string()));
        assert_eq!(row.get::<_, Option<String>>(1), Some("atomic-spawn".into()));
        assert_eq!(row.get::<_, Option<String>>(2), Some(sha256_hex(raw)));
        assert_eq!(row.get::<_, String>(3), "Completed");
        assert_eq!(row.get::<_, DateTime<Utc>>(4), persisted_spawned_at);
        client
            .batch_execute("DROP TABLE spawn_records")
            .await
            .expect("remove disposable table");
        drop(client);
        driver.await.expect("join PG driver");
    }
}
