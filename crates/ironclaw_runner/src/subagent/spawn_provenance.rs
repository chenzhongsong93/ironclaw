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

use super::await_edge::EdgeTerminalKind;

/// 铁律(2026-08-07):通用平台能力不做领域判断——本扩展只做**忠实文件持久化**
/// (子 agent final_text 原文落盘 + raw sha256),零小说领域假设(剥围栏/判正文/拒汇报
/// 等全属天权业务逻辑,在天权 api 读取侧 `read_prose_from_ssot_file` 实现)。
/// 落盘 raw = 完整过程记录(可 debug),天权读取时提取纯正文用于呈现/校验。

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
    /// raw final_text 的 sha256(通用契约:天权 validator 用 LLM 传的 prose 原文比对此值,
    /// 证明 prose 真来自 spawn;领域提取不参与此 hash)。
    pub final_text_hash: Option<String>,
    /// sanitized terminal reason(失败类别,如 iteration_limit / driver_protocol_violation)。
    /// 通用契约:settle 侧 event.sanitized_reason 忠实落库,零领域判断;天权侧按它归类
    /// 失败 spawn(ISSUE-IRONCLAW-009 留观项:child loop-exit 空转的失败归类)。
    /// Completed 恒 None。
    pub failure_category: Option<String>,
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
        failure_category: Option<String>,
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
    persist_final_text_to_workspace(final_text, record);
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

/// 把子 agent final_text 原文忠实落盘到 workspace 正文文件(引擎侧落盘,零 LLM 转抄)。
///
/// 铁律(2026-08-07):通用平台只做忠实持久化——落盘 raw final_text 原文,不做领域判断
/// (剥围栏/判正文/拒汇报全属天权业务逻辑,在天权 api 读取侧 `read_prose_from_ssot_file`
/// 提取纯正文用于呈现/校验)。raw 落盘 = 完整过程记录(可 debug)。
/// 路径:{TIANQUAN_SPAWN_PROSE_DIR}/{project_id}/chapters/ch24.txt
/// project_id 固定 "iron-city"(子 agent scope.project_id 为 None,spawn 链路不传 project;
/// 当前验证场景固定 iron-city,generalize 时需在 spawn 记录里带 project_id/chapter_no,见
/// debt-register 落盘债)。env 未设/写失败 → warn 日志跳过(settle 路径不受影响)。
fn persist_final_text_to_workspace(final_text: Option<&str>, record: &SpawnProvenanceRecord) {
    let Some(prose_dir) = std::env::var("TIANQUAN_SPAWN_PROSE_DIR").ok() else {
        return;
    };
    let Some(prose) = final_text else {
        return;
    };
    let project_id = record.project_id.as_deref().unwrap_or("iron-city");
    let dir = std::path::Path::new(&prose_dir)
        .join(project_id)
        .join("chapters");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            target: "tianquan_spawn_provenance",
            child_run_id = %record.child_run_id,
            dir = %dir.display(),
            error = %error,
            "spawn prose dir create failed (best-effort, settle path unaffected)"
        );
        return;
    }
    let path = dir.join("ch24.txt");
    if let Err(error) = std::fs::write(&path, &prose) {
        tracing::warn!(
            target: "tianquan_spawn_provenance",
            child_run_id = %record.child_run_id,
            path = %path.display(),
            error = %error,
            "spawn prose write failed (best-effort, settle path unaffected)"
        );
    } else {
        tracing::info!(
            target: "tianquan_spawn_provenance",
            child_run_id = %record.child_run_id,
            path = %path.display(),
            byte_len = prose.len(),
            "spawn prose persisted to workspace"
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
             (child_run_id, subagent_type, project_id, layer, spawned_at, terminal_status, final_text_hash, failure_category) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (child_run_id) DO UPDATE SET \
             subagent_type = EXCLUDED.subagent_type, \
             project_id = EXCLUDED.project_id, \
             layer = EXCLUDED.layer, \
             spawned_at = EXCLUDED.spawned_at, \
             terminal_status = EXCLUDED.terminal_status, \
             final_text_hash = EXCLUDED.final_text_hash, \
             failure_category = EXCLUDED.failure_category",
            &[
                &record.child_run_id,
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

    // 覆盖:§2.2 PH-IC-MAP-02 — final_text=None → final_text_hash=None;
    // Some → raw final_text 的 sha256(铁律:忠实持久化,不剥围栏不做领域判断)
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
            None,
        );
        assert_eq!(rec.final_text_hash, None);
        // 忠实持久化:任意 final_text(含围栏/汇报)hash = raw 的 sha256,不做领域判断
        let direct_prose = "夜色压城。".repeat(30); // ≥100 字
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
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
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
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
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
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
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
            &scope_some,
            &kind("general"),
            EdgeTerminalKind::Failed,
            None,
            Utc::now(),
            None,
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
            None,
        );
        // 无 PG 在跑;若误触网必 panic/超时,返回即证明零开销跳过
        record_spawn_terminal(None, &rec, None).await;
    }

    // 覆盖:2026-08-04 落盘标准 + 2026-08-07 铁律——raw final_text 忠实落盘(通用平台不做领域判断)
    #[test]
    fn persist_final_text_writes_ch24_txt_when_env_set() {
        // 用一次性临时目录避免污染真实 workspace
        let tmp = std::env::temp_dir().join(format!("tq-spawn-prose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe {
            std::env::set_var("TIANQUAN_SPAWN_PROSE_DIR", &tmp);
        }
        let child_run_id = TurnRunId::new();
        // project_id=None 走 fallback "iron-city"(子 agent scope 不带 project,2026-08-04)
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
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
        let direct_path = tmp.join("iron-city").join("chapters").join("ch24.txt");
        assert!(direct_path.exists(), "正文直出应落盘");
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
        persist_final_text_to_workspace(Some(&report), &rec);
        let written_report = std::fs::read_to_string(&direct_path).expect("读回");
        assert_eq!(
            written_report, report,
            "忠实落盘:汇报文本也原样落盘(领域判断不在通用平台)"
        );

        // 围栏形态同样忠实落盘 = raw 全文(剥围栏在天权 api 读取侧)
        let prose = "夜".repeat(200);
        let fenced =
            format!("```json\n{{\"prose\": \"{prose}\", \"usedGraphFacts\": [\"F1\"]}}\n```");
        persist_final_text_to_workspace(Some(&fenced), &rec);
        let written_fenced = std::fs::read_to_string(&direct_path).expect("读回");
        assert_eq!(
            written_fenced, fenced,
            "忠实落盘:围栏形态 = raw 全文(不剥围栏)"
        );
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe {
            std::env::remove_var("TIANQUAN_SPAWN_PROSE_DIR");
        }
    }

    // 覆盖:ISSUE-IRONCLAW-009 留观项收口条件①——失败终态的 sanitized reason 忠实落库
    // (failure_category),Completed 恒 None;天权侧按它归类失败 spawn,不再依赖已丢的网关日志。
    #[test]
    fn record_from_terminal_captures_failure_category() {
        let child_run_id = TurnRunId::new();
        let scope = scope_with_project(None);
        // Failed + 非空 reason → 记录
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
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
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Failed,
            None,
            Utc::now(),
            Some("   ".to_string()),
        );
        assert_eq!(rec.failure_category, None);
        // Completed → 恒 None(即便传入也不记录,Completed 无失败类别)
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
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
            let rec = SpawnProvenanceRecord::from_terminal(
                &child_run_id,
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
}
