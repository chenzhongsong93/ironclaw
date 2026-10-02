//! C-HOOKS (+ E-HOOK-INFRA): a wired `hook_dispatcher_builder_factory` should
//! fire hooks at the expected lifecycle points on a real coordinator-path turn,
//! and a hook deny should block the capability without wedging the run.
//!
//! These drive a full coordinator-path turn with an active hook dispatcher —
//! the first tests to do so — so they also pin that `HookedLoopCheckpointPort`
//! stays transparent for `stage_checkpoint_payload`/`load_checkpoint_payload`,
//! not just `checkpoint`. A planned run stages a checkpoint payload before
//! every model call, so any gap there fails every hooks-enabled turn.

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

use ironclaw_events::{SecurityBoundary, SecurityDecision};
use ironclaw_hooks::dispatch::HOOK_DENY_PREDICATE_CODE;
use reborn_support::assertions::ToolErrorClass;
use reborn_support::builder::RebornIntegrationHarness;
use reborn_support::hooks::{
    HOOK_TEST_DENY_REASON, RecordingHookLog, denying_hook_factory, recording_hook_factory,
};
use reborn_support::reply::RebornScriptedReply;
use serde_json::json;

const HTTP_TOOL_URL: &str = "https://api.example.test/v1/items";

// 覆盖：TianQuan TEST-SPEC §ISSUE023 AE-L2-121；真实包装caller，非guard helper。
#[tokio::test]
async fn tianquan_repeat_guard_counts_activities_not_backend_replays() {
    let server = support::mock_mcp_server::start_mock_mcp_server(vec![
        support::mock_mcp_server::MockToolResponse {
            name: "search".to_string(),
            content: json!({"results": []}),
        },
    ])
    .await;
    server.force_http_status(500);
    let evaluations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let h = RebornIntegrationHarness::test_default()
        .with_mock_mcp(server.mcp_url())
        .with_hook_factory(tianquan_counting_factory(evaluations.clone()).await)
        .script([
            RebornScriptedReply::tool_call("mock-mcp.search", json!({"query": "same"})),
            RebornScriptedReply::tool_call("mock-mcp.search", json!({"query": "same"})),
            RebornScriptedReply::text("done"),
        ])
        .build()
        .await
        .expect("harness builds");
    h.submit_turn("review then bounded retry")
        .await
        .expect("turn completes");
    let seen = evaluations.lock().expect("evaluation log").clone();
    assert_eq!(
        seen.len(),
        6,
        "initial batch and two recovery retries per model call"
    );
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[1], seen[2]);
    assert_eq!(seen[3], seen[4]);
    assert_eq!(seen[4], seen[5]);
    assert_ne!(seen[0], seen[3], "new model call must mint a new activity");
    assert_eq!(
        server
            .recorded_requests()
            .iter()
            .filter(|r| r.method == "tools/call")
            .count(),
        2
    );
    h.assert_tool_error(ToolErrorClass::Failed, "backend")
        .await
        .expect("backend visible");
    h.assert_no_tool_error(ToolErrorClass::Denied, "policy_denied")
        .await
        .expect("two activities must not exhaust the six-activity guard");
    let requests = h.scripted_llm.captured_requests();
    let tool_results: Vec<_> = requests
        .last()
        .expect("final model request")
        .iter()
        .filter(|message| message.role == ironclaw_llm::Role::Tool)
        .collect();
    assert_eq!(tool_results.len(), 2, "both activities reach the model");
    for result in tool_results {
        assert!(result.content.contains("backend"));
        assert!(!result.content.contains("policy_denied"));
    }
    server.shutdown().await;
}

// 覆盖：AE-L2-121 IC23-LIMIT/SCOPE；同factory跨真实run创建fresh guard。
#[tokio::test]
async fn tianquan_repeat_guard_keeps_six_limit_and_resets_between_runs() {
    let server = support::mock_mcp_server::start_mock_mcp_server(vec![
        support::mock_mcp_server::MockToolResponse {
            name: "search".to_string(),
            content: json!({"results": ["ok"]}),
        },
    ])
    .await;
    let evaluations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut script: Vec<_> = (0..6)
        .map(|i| {
            RebornScriptedReply::tool_call(
                "mock-mcp.search",
                json!({"query": format!("query-{i}")}),
            )
        })
        .collect();
    script.extend([
        RebornScriptedReply::text("bounded done"),
        RebornScriptedReply::tool_call("mock-mcp.search", json!({"query": "new-run"})),
        RebornScriptedReply::text("fresh done"),
    ]);
    let h = RebornIntegrationHarness::test_default()
        .with_mock_mcp(server.mcp_url())
        .with_hook_factory(tianquan_counting_factory(evaluations.clone()).await)
        .script(script)
        .build()
        .await
        .expect("harness builds");
    h.submit_turn("six distinct calls")
        .await
        .expect("first run");
    h.assert_tool_error(ToolErrorClass::Denied, "policy_denied")
        .await
        .expect("sixth denied");
    assert_eq!(
        server
            .recorded_requests()
            .iter()
            .filter(|r| r.method == "tools/call")
            .count(),
        5
    );
    let baseline = h.history_len().await.expect("first run history");
    h.submit_turn("fresh run").await.expect("second run");
    h.assert_no_tool_error_since(baseline, ToolErrorClass::Denied, "policy_denied")
        .await
        .expect("next run has no stale denial");
    assert_eq!(
        server
            .recorded_requests()
            .iter()
            .filter(|r| r.method == "tools/call")
            .count(),
        6
    );
    let seen = evaluations.lock().expect("evaluation log").clone();
    assert_eq!(seen.len(), 7);
    assert_eq!(
        seen.into_iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        7,
        "six first-run activities and the next-run activity are distinct"
    );
    server.shutdown().await;
}

async fn tianquan_counting_factory(
    evaluations: std::sync::Arc<std::sync::Mutex<Vec<ironclaw_turns::CapabilityActivityId>>>,
) -> ironclaw_runner::loop_driver_host::HookDispatcherBuilderFactory {
    use ironclaw_reborn_composition::{
        HooksActivationConfig, build_hook_dispatcher_builder_factory,
        build_hook_projection_registry,
    };
    let config = HooksActivationConfig::enabled();
    let projection = build_hook_projection_registry::<ironclaw_filesystem::InMemoryBackend>(
        ironclaw_extensions::ExtensionRegistry::new(),
        None,
        config,
    )
    .await
    .expect("projection");
    let factory = build_hook_dispatcher_builder_factory(config, &projection)
        .expect("production factory")
        .expect("hooks enabled");
    std::sync::Arc::new(move || {
        factory()?
            .install_builtin_before_capability(
                ironclaw_hooks::identity::HookId::for_builtin(
                    "test::activity-recorder",
                    ironclaw_hooks::identity::HookVersion::ONE,
                ),
                ironclaw_hooks::ordering::HookPhase::Validation,
                Box::new(ActivityRecorder(evaluations.clone())),
            )
            .map_err(|error| {
                ironclaw_runner::loop_driver_host::RebornLoopDriverHostError::InvalidRequest {
                    reason: error.to_string(),
                }
            })
    })
}

struct ActivityRecorder(
    std::sync::Arc<std::sync::Mutex<Vec<ironclaw_turns::CapabilityActivityId>>>,
);

#[async_trait::async_trait]
impl ironclaw_hooks::sink::PrivilegedBeforeCapabilityHook for ActivityRecorder {
    async fn evaluate(
        &self,
        ctx: &ironclaw_hooks::points::BeforeCapabilityHookContext,
        sink: &mut dyn ironclaw_hooks::sink::PrivilegedGateSink,
    ) {
        self.0
            .lock()
            .expect("evaluation log")
            .push(ctx.activity_id.expect("caller activity"));
        sink.pass();
    }
    fn needs_input(&self) -> bool {
        false
    }
}

/// The BeforeCapability gate hook fires before the dispatched capability, and
/// the AfterModel observer fires once for the turn — both recorded through
/// the real turn wire. The passing gate hook does not block the capability,
/// so the http tool still runs.
#[tokio::test]
async fn hooks_fire_at_lifecycle_points_on_coordinator_turn() {
    let log = RecordingHookLog::new();
    let h = RebornIntegrationHarness::test_default()
        .with_builtin_http_tools()
        .with_hook_factory(recording_hook_factory(log.clone()))
        .script([
            RebornScriptedReply::tool_call("builtin.http", json!({"url": HTTP_TOOL_URL})),
            RebornScriptedReply::text("done"),
        ])
        .build()
        .await
        .expect("harness builds");
    h.submit_turn("fetch items").await.expect("turn completes");

    h.assert_tool_invoked("builtin.http")
        .await
        .expect("http tool ran through the real capability path");
    // AfterModel fires only once per turn, at `finalize_assistant_message`
    // for the terminal text reply — the tool-call reply that precedes it
    // finalizes through the capability path, not the transcript port, so it
    // does not fire AfterModel on its own.
    assert_eq!(
        log.fires(),
        vec!["before_capability:builtin.http", "observer:AfterModel",],
        "hook fires must occur in lifecycle order: BeforeCapability (builtin.http dispatch) \
         -> AfterModel (final text reply)"
    );
}

/// A BeforeCapability hook deny should block the capability (it never reaches the
/// wire) yet the run should still complete — the hook error path must NOT wedge
/// the run.
#[tokio::test]
async fn hook_deny_blocks_capability_without_wedging_run() {
    let log = RecordingHookLog::new();
    let h = RebornIntegrationHarness::test_default()
        .with_builtin_http_tools()
        .with_hook_factory(denying_hook_factory(log.clone(), "builtin.http"))
        .script([
            RebornScriptedReply::tool_call("builtin.http", json!({"url": HTTP_TOOL_URL})),
            RebornScriptedReply::text("done"),
        ])
        .build()
        .await
        .expect("harness builds");
    // `submit_turn` waits for `Completed`: reaching it proves the deny did not
    // wedge the run (a wedged/failed run would fail this wait).
    h.submit_turn("fetch items")
        .await
        .expect("turn completes despite the hook deny");

    assert!(
        log.fired("before_capability_deny:builtin.http"),
        "deny hook must fire for builtin.http; saw {:?}",
        log.fires()
    );
    // The denied capability never reached the HTTP wire (blocked before the
    // inner runtime port), so no egress was captured.
    h.assert_egress_count(0)
        .await
        .expect("a hook-denied capability must not reach egress");
    // The model-visible tool-result envelope reports the hook's deny reason,
    // not a generic/blank denial — pins that the deny reason token actually
    // propagates to the persisted `ToolResultReference` the model sees.
    h.assert_tool_error(ToolErrorClass::Denied, HOOK_TEST_DENY_REASON)
        .await
        .expect("hook deny reason must be reported in the persisted tool-error summary");
    h.assert_security_audit_event_recorded(
        SecurityBoundary::HookDeny,
        SecurityDecision::Blocked,
        HOOK_DENY_PREDICATE_CODE,
    )
    .await
    .expect("hook deny must record a security-audit event through the harness recorder");
}
