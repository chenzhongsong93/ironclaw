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
//! 2026-08-07 治本:`final_text_hash` 是**提取正文**(剥 ```json``` 围栏)的 sha256,
//! 非 raw final_text——无正文产物(汇报文本/无围栏)hash=None,api 校验必然 block,
//! 杜绝"非正文被 accept"假正。落盘文件同样只写提取正文(纯正文 txt)。
//!
//! Zero-cost when disabled: `TIANQUAN_SPAWN_PG_URL` unset → every call is an
//! immediate no-op (upstream IronClaw never sets it; only the TianQuan
//! docker-compose gateway service does). All errors are logged, never
//! propagated — provenance recording must never break the settle path.

use chrono::{DateTime, Utc};
use ironclaw_turns::{TurnRunId, TurnScope, TurnTimestamp};
use sha2::Digest as _;

use super::await_edge::EdgeTerminalKind;

/// 从子 agent final_text 提取正文(2026-08-07 治本:落盘/哈希只用正文,不落 raw final_text)。
///
/// 协议(l8-delegate-goal.md):子 agent "直接输出正文 + 末尾 ```json``` 围栏",围栏内 JSON
/// 含 `prose`(协议字段)+ usedGraphFacts/dnaTechniquesUsed/newFactCandidates。
/// 提取规则:
/// - 找到 ``` 围栏 → 剥围栏解析 JSON → 取 `prose`(协议字段)或 `final_text`(子 agent 变体字段)
/// - 无围栏 / 解析失败 / 无正文字段 / 正文 <100 字 → None
///   (视为非正文产物——如"状态汇报"文本——不落盘 + final_text_hash=None → api validator block)
/// 返回剥离围栏后的纯正文。
pub(crate) fn extract_prose_from_final_text(final_text: &str) -> Option<String> {
    let start = final_text.find("```")?;
    let rest = &final_text[start + 3..];
    let end = rest.find("```")?;
    let json_block = rest[..end].trim();
    let json_str = json_block
        .strip_prefix("json")
        .map(str::trim)
        .unwrap_or(json_block);
    let value: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let prose = value
        .get("prose")
        .and_then(|p| p.as_str())
        .or_else(|| value.get("final_text").and_then(|p| p.as_str()))?
        .trim();
    if prose.chars().count() < 100 {
        return None; // 正文过短(<100 字)视为未产出,与 api 100 字下限一致
    }
    Some(prose.to_string())
}

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
            // 2026-08-07 治本:final_text_hash = 提取正文的 hash(剥 ```json``` 围栏),
            // 非 raw final_text。无正文(汇报文本/无围栏)→ None → api validator hash 比对
            // 必然不匹配 → block(杜绝"汇报文本被 accept"假正)。
            final_text_hash: final_text
                .and_then(extract_prose_from_final_text)
                .map(|prose| sha256_hex(&prose)),
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
/// api (`spawn_store::sha256_hex`); both sides hash the extracted prose
/// (2026-08-07:正文 hash,剥 ```json``` 围栏后,非 raw final_text)。
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

/// 把子 agent 正文落盘到 workspace 正文文件(引擎侧落盘,零 LLM 转抄)。
///
/// 2026-08-07 治本:落盘前经 [`extract_prose_from_final_text`] 提取正文
/// (剥 ```json``` 围栏),落盘文件 = 纯正文 txt;无正文产物(汇报文本/无围栏/过短)
/// → 不落盘 + final_text_hash=None → api validator hash 门 block。
/// 路径:{TIANQUAN_SPAWN_PROSE_DIR}/{project_id}/chapters/ch24.txt
/// project_id 固定 "iron-city"(子 agent scope.project_id 为 None,spawn 链路不传 project;
/// 当前验证场景固定 iron-city,generalize 时需在 spawn 记录里带 project_id/chapter_no,见
/// debt-register 落盘债)。env 未设/写失败 → warn 日志跳过(settle 路径不受影响)。
fn persist_final_text_to_workspace(final_text: Option<&str>, record: &SpawnProvenanceRecord) {
    let Some(prose_dir) = std::env::var("TIANQUAN_SPAWN_PROSE_DIR").ok() else {
        return;
    };
    let Some(prose) = final_text.and_then(extract_prose_from_final_text) else {
        tracing::warn!(
            target: "tianquan_spawn_provenance",
            child_run_id = %record.child_run_id,
            "spawn final_text 无正文(无 ```json``` 围栏/无 prose 字段/过短),不落盘(validator 将按 hash None block)"
        );
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

    // 覆盖:§2.2 PH-IC-MAP-02 — final_text=None → final_text_hash=None;
    // Some → 提取正文(剥 ```json``` 围栏)后的 sha256(2026-08-07 治本)
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
        // 无围栏的纯文本(非协议产物)→ 无正文 → hash None
        let rec = SpawnProvenanceRecord::from_terminal(
            &child_run_id,
            &scope,
            &kind("novelist"),
            EdgeTerminalKind::Completed,
            Some("夜色压城。"),
            spawned_at,
        );
        assert_eq!(
            rec.final_text_hash, None,
            "无 ```json``` 围栏 = 非协议产物,hash None"
        );
        // 协议形态(正文 + ```json``` 围栏,prose 字段)→ hash = 提取正文的 sha256
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
        );
        assert_eq!(
            rec.final_text_hash.as_deref(),
            Some(sha256_hex(&prose).as_str()),
            "hash = 提取正文的 sha256(非 raw final_text)"
        );
        assert_eq!(rec.child_run_id, child_run_id.to_string());
        assert_eq!(rec.subagent_type, "novelist");
        assert_eq!(rec.terminal_status, "Completed");
        assert_eq!(rec.spawned_at, spawned_at);
        assert_eq!(rec.layer, None);
    }

    // 覆盖:2026-08-07 治本——extract_prose_from_final_text 提取正文(剥围栏)
    #[test]
    fn extract_prose_from_fenced_final_text() {
        let prose = "夜".repeat(200);
        let ft = format!("```json\n{{\"prose\": \"{prose}\", \"usedGraphFacts\": [\"F1\"]}}\n```");
        let got = extract_prose_from_final_text(&ft);
        assert_eq!(got.as_deref(), Some(prose.as_str()));
    }

    #[test]
    fn extract_prose_from_final_text_field_variant() {
        // 子 agent 变体:围栏内用 final_text 字段名(2026-08-07 实测第一轮落盘形态)
        let prose = "夜".repeat(150);
        let ft = format!("```json\n{{\"final_text\": \"{prose}\"}}\n```");
        assert_eq!(
            extract_prose_from_final_text(&ft).as_deref(),
            Some(prose.as_str())
        );
    }

    #[test]
    fn extract_prose_none_without_fence() {
        // 无围栏 = 非协议产物(如"状态汇报"文本)→ None
        assert_eq!(
            extract_prose_from_final_text("当前状态汇报: spawn 被拒,fanout 超限"),
            None
        );
    }

    #[test]
    fn extract_prose_none_when_prose_too_short() {
        let ft = "```json\n{\"prose\": \"太短\"}\n```";
        assert_eq!(
            extract_prose_from_final_text(ft),
            None,
            "正文 <100 字 → None"
        );
    }

    #[test]
    fn extract_prose_none_when_json_invalid() {
        let ft = "```json\n{这不是合法 JSON\n```";
        assert_eq!(extract_prose_from_final_text(ft), None);
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
        record_spawn_terminal(None, &rec, None).await;
    }

    // 覆盖:2026-08-04 落盘标准 + 2026-08-07 治本——正文落盘到 workspace(剥围栏纯正文)
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
        );
        // 无围栏文本(非协议产物)→ 不落盘(2026-08-07 治本,原"短正文不落盘"语义扩展)
        persist_final_text_to_workspace(Some("短正文"), &rec);
        let short_path = tmp.join("iron-city").join("chapters").join("ch24.txt");
        assert!(!short_path.exists(), "无围栏/过短不应落盘");

        // 汇报文本(无围栏,≥100 字)→ 不落盘(2026-08-07 治本:非正文产物)
        let report = "当前状态汇报: spawn 被拒 fanout 超限,等待配额释放。".repeat(6);
        assert!(
            report.chars().count() >= 100,
            "汇报文本应 ≥100 字以测试非长度拦截"
        );
        persist_final_text_to_workspace(Some(&report), &rec);
        assert!(!short_path.exists(), "汇报文本(无 ```json``` 围栏)不应落盘");

        // 协议形态(正文 + ```json``` 围栏)→ 落盘 = 纯正文(剥围栏)
        let prose = "夜".repeat(200);
        let fenced =
            format!("```json\n{{\"prose\": \"{prose}\", \"usedGraphFacts\": [\"F1\"]}}\n```");
        persist_final_text_to_workspace(Some(&fenced), &rec);
        let long_path = tmp.join("iron-city").join("chapters").join("ch24.txt");
        assert!(long_path.exists(), "围栏正文应落盘");
        let written = std::fs::read_to_string(&long_path).expect("读回");
        assert_eq!(
            written, prose,
            "落盘内容 = 提取正文(剥围栏),非 raw final_text"
        );
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe {
            std::env::remove_var("TIANQUAN_SPAWN_PROSE_DIR");
        }
    }
}
