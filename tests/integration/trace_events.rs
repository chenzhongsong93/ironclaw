//! Caller-path contract for Reborn model/tool trace events.

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

use reborn_support::{group::RebornIntegrationGroup, reply::RebornScriptedReply};
use serde_json::{Value, json};

struct TraceRootGuard(Option<std::ffi::OsString>);

impl TraceRootGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("TIANQUAN_TRACE_DIR");
        unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", path) };
        Self(previous)
    }
}

impl Drop for TraceRootGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            unsafe { std::env::set_var("TIANQUAN_TRACE_DIR", previous) };
        } else {
            unsafe { std::env::remove_var("TIANQUAN_TRACE_DIR") };
        }
    }
}

#[tokio::test]
async fn reborn_tool_call_writes_correlated_request_and_response_trace() {
    let trace_root = tempfile::tempdir().expect("isolated trace root");
    let _trace_root_guard = TraceRootGuard::set(trace_root.path());
    let group = RebornIntegrationGroup::builtin_tools()
        .await
        .expect("builtin tool group builds");
    let harness = group
        .thread("trace-event-contract")
        .script([
            RebornScriptedReply::tool_call(
                "builtin.time",
                json!({"operation":"parse","input":1_778_590_800_123_i64}),
            ),
            RebornScriptedReply::text("time parsed"),
        ])
        .build()
        .await
        .expect("thread builds");

    let run_id = harness
        .submit_turn("parse this timestamp")
        .await
        .expect("turn completes");
    harness
        .assert_tool_invoked("builtin.time")
        .await
        .expect("time capability ran");

    let hot = trace_root.path().join(format!("{run_id}.jsonl"));
    let archive = trace_root.path().join(format!("{run_id}.jsonl.gz"));
    let trace = if archive.exists() {
        let bytes = std::fs::read(archive).expect("read completed run archive");
        let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut text = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut text).expect("decompress trace");
        text
    } else {
        std::fs::read_to_string(hot).expect("read active run trace")
    };
    let events = trace
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("trace event is valid JSON"))
        .collect::<Vec<_>>();
    let kinds = events
        .iter()
        .filter_map(|event| event["kind"].as_str())
        .collect::<Vec<_>>();
    // This in-process harness injects at the scripted-provider SDK seam; the
    // production gateway's model_exchange events are checked in the live
    // isolated Gateway dossier verification.
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "tool_request").count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == "tool_response")
            .count(),
        1
    );

    let request = events
        .iter()
        .find(|event| event["kind"] == "tool_request")
        .expect("tool request trace exists");
    let response = events
        .iter()
        .find(|event| event["kind"] == "tool_response")
        .expect("tool response trace exists");
    assert_eq!(request["data"]["capability_id"], "builtin.time");
    assert_eq!(request["data"]["input"]["operation"], "parse");
    assert_eq!(
        request["data"]["invocation_id"],
        response["data"]["invocation_id"]
    );
    assert!(response["data"]["output"].is_object());
}
