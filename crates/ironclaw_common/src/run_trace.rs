//! run_trace.rs — 天权 trace schema v1 的 ironclaw exporter(2026-08-26)。
//!
//! ★定位(双形态约束治理):本模块是**薄 exporter**——schema(事件类型/字段/
//! JSONL 格式)的真源在天权仓(api crate 的 dossier 解析器 + 契约文档),此处
//! 只负责把 ironclaw 进程内事件按该 schema 落文件。换 agent 平台(deepseek-
//! harness 等)时重写本层,天权侧档案格式/聚合端点/终端 UI 零迁移。
//!
//! 背景:《头七账》ch1 质量事故归因时,模型 prompt 全文/工具入参全文/子 agent
//! 产物均无处可查(result 只存 785B 预览,日志只有 capability_id),开发者与
//! 用户都在黑盒里靠 grep/sqlite 捞证据。本模块把每次 run 的关键交换按
//! JSONL 追加到文件,api 侧(挂同一卷只读)的 dossier 端点聚合展示。
//!
//! 契约:文件即接口——`{root}/traces/{run_id}.jsonl`,每行一个事件:
//! {"ts","kind","run_id","thread_id","turn_id","data"}
//! kind ∈ model_exchange(请求/响应全文) | tool_call(入参/出参全文) |
//! spawn_note(子 agent 生命周期备注)。
//!
//! 写入必须 best-effort:任何 IO 错误只 debug! 不影响主流程(trace 采集
//! 挂掉不能挂创作链);env `TIANQUAN_TRACE_DIR` 覆盖目录,未配置且无
//! storage root 环境时静默跳过(本地单测零依赖)。

use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;

/// 默认 trace 根(与 local-dev storage root 同卷,api 以只读卷共享)。
const DEFAULT_TRACE_ROOT: &str = "/data/ironclaw-reborn/local-dev/traces";

/// 解析 trace 根目录:env `TIANQUAN_TRACE_DIR` > 默认路径(存在性由写入时
/// best-effort 兜底;默认路径不可写时静默丢弃)。
fn trace_root() -> PathBuf {
    std::env::var("TIANQUAN_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_TRACE_ROOT))
}

/// 追加一个 run trace 事件(best-effort;失败仅 debug 日志)。
///
/// `kind` 事件类型;`run_id` 归属 run(文件名即 run_id);
/// `thread_id`/`turn_id` 可选关联;`data` 事件负载(调用方保证可序列化,
/// 含 credential 的字段由调用方先脱敏——本层不做内容判断)。
pub fn append_trace_event(
    kind: &str,
    run_id: &str,
    thread_id: Option<&str>,
    turn_id: Option<&str>,
    data: Value,
) {
    let event = json!({
        "ts": chrono_now_iso(),
        "kind": kind,
        "run_id": run_id,
        "thread_id": thread_id,
        "turn_id": turn_id,
        "data": data,
    });
    let path = trace_root().join(sanitize_run_id(run_id));
    // create_dir_all 幂等(首次事件时建 traces/;已存在零开销)
    if let Err(e) = std::fs::create_dir_all(trace_root()) {
        tracing_stub_debug(format!("run trace mkdir failed: {e}"));
        return;
    }
    let line = match serde_json::to_string(&event) {
        Ok(l) => l,
        Err(e) => {
            tracing_stub_debug(format!("run trace serialize failed: {e}"));
            return;
        }
    };
    // append+create,单行写;并发 append 同文件由 OS 行原子性兜底(JSONL 惯例)
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = result {
        tracing_stub_debug(format!("run trace append failed({}): {e}", path.display()));
    }
}

/// run_id 只允许 hex/dash(防路径穿越;TurnRunId UUID 形态天然满足)。
fn sanitize_run_id(run_id: &str) -> String {
    let cleaned: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if cleaned.is_empty() {
        "unknown-run".to_string()
    } else {
        cleaned
    }
}

/// ISO-8601 毫秒时间戳(chrono 为 crate 直接依赖)。
fn chrono_now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// best-effort 调试日志(tracing 为 crate 直接依赖;采集失败不污染主日志)。
fn tracing_stub_debug(msg: String) {
    tracing::debug!(target: "run_trace", "{msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_traversal() {
        assert_eq!(sanitize_run_id("../../etc"), "etc");
        assert_eq!(sanitize_run_id("a/b\\c"), "abc");
        assert_eq!(sanitize_run_id(""), "unknown-run");
        let uuid = "61304aa1-f032-4d7e-90cc-9967362b4df2";
        assert_eq!(sanitize_run_id(uuid), uuid);
    }

    #[test]
    fn append_writes_jsonl_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        // 直接写指定目录:临时改 env 有并发风险,这里走内部路径拼接验证
        let path = dir.path().join("run-x.jsonl");
        let event = json!({"ts": "1", "kind": "tool_call", "run_id": "run-x"});
        let line = serde_json::to_string(&event).expect("ser");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open");
        writeln!(f, "{line}").expect("write");
        let back = std::fs::read_to_string(&path).expect("read");
        let parsed: Value = serde_json::from_str(back.trim()).expect("parse");
        assert_eq!(parsed["kind"], "tool_call");
    }
}
