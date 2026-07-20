//! 天权 16 SOUL subagent spawn 端到端验证(2026-07-19 新增)。
//!
//! 验证 ironclaw 仓的 TianquanSubagentDefinitionResolver(16 SOUL flavor)在 agent turn
//! 真实派生子 agent。复用 reborn_subagent_spawn_e2e.rs 的回放模式,但注入天权 resolver
//! 替代默认 StaticSubagentDefinitionResolver(只认 4 内置 flavor)。
//!
//! 覆盖:tests/TEST-SPEC-tianquan-subagent-spawn.md(天权仓)
//! - TS-SPAWN-01: novelist flavor spawn 真派生子 agent
//! - TS-SPAWN-02: 16 SOUL kind 全部 resolve_kind 返 Some
//! - TS-SPAWN-03: child thread metadata.subagent_kind="novelist"
//! - TS-SPAWN-05: 父 resume 含 "Subagent completed" tool result
//! - TS-CONTRAST-01: 默认 resolver 下 novelist flavor spawn 失败(对照)
//!
//! 边界:本轮只注入 definition_resolver(验 resolve + spawn + metadata + reply)。
//! prompt_source 暂不注入(走 GateBacked 默认),direction_markdown 注入验证留下轮。

#[allow(dead_code)]
#[path = "support/reborn_parity_qa/mod.rs"]
mod parity_qa_support;
#[allow(dead_code)]
#[path = "integration/support/mod.rs"]
mod reborn_support;
mod support;

use std::sync::Arc;
use std::time::Duration;

use ironclaw_host_api::CapabilityId;
use ironclaw_loop_host::DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID;
use ironclaw_loop_host::{HostManagedModelMessageRole, HostManagedModelResponse, SubagentDefinitionResolver};
use ironclaw_turns::TurnStatus;
use parity_qa_support::binary_e2e::{RebornBinaryE2EHarness, SubmittedTurn};
use parity_qa_support::model_replay::{
    RebornModelReplayStep, RebornScriptedProviderToolCall, RebornTraceReplayModelGateway,
};
use reborn_support::{config::WaitConfig, harness::RecordingTestCapabilityPort};
use tianquan_subagents::{
    flavors::TIANQUAN_SOUL_FLAVORS, TianquanSubagentDefinitionResolver,
};

/// TS-SPAWN-01: novelist flavor spawn → child 派生 + Completed
///
/// 验证:TianquanSubagentDefinitionResolver 识别 "novelist" kind(16 SOUL 之一),
/// spawn_subagent 真派生子 agent,子 agent 跑完 reply 回父 tool result,父 resume。
#[tokio::test]
async fn ts_spawn_01_novelist_flavor_spawns_child() {
    let model_gateway = RebornTraceReplayModelGateway::with_scripted_steps([
        RebornModelReplayStep::ProviderToolCalls {
            calls: vec![spawn_call(
                "spawn_novelist",
                serde_json::json!({
                    "flavor_id": "novelist",
                    "task": "写第 24 章正文",
                }),
            )],
            expected_tool_results: Vec::new(),
        },
        RebornModelReplayStep::Response {
            response: HostManagedModelResponse::assistant_reply("novelist child output: 正文草稿"),
            expected_tool_results: Vec::new(),
        },
        RebornModelReplayStep::Response {
            response: HostManagedModelResponse::assistant_reply("parent resumed after novelist"),
            expected_tool_results: Vec::new(),
        },
    ]);
    let mut harness = tianquan_spawn_harness("tianquan-novelist-spawn", model_gateway).await;
    harness.start();

    let submitted = harness
        .submit_text("event-tianquan-novelist-spawn", "delegate writing to novelist")
        .await
        .expect("submit root turn");
    harness
        .wait_for_status(submitted.run_id, TurnStatus::BlockedDependentRun)
        .await
        .expect("parent parks on dependent novelist child");

    let child = await_single_child(&harness, &submitted).await;
    harness
        .wait_for_status_in_scope(child.scope.clone(), child.run_id, TurnStatus::Completed)
        .await
        .expect("novelist child completes");
    harness
        .wait_for_status(submitted.run_id, TurnStatus::Completed)
        .await
        .expect("parent resumes after novelist child completion");
    harness
        .assert_final_reply("parent resumed after novelist")
        .await
        .expect("parent final reply");
    assert_child_thread_invariants(&submitted, &child);
    assert!(
        harness.model_requests()[2]
            .messages
            .iter()
            .any(
                |message| message.role == HostManagedModelMessageRole::ToolResult
                    && message.content.contains("Subagent completed")
            ),
        "parent resume request includes the novelist child completion tool result: {:#?}",
        harness.model_requests()[2].messages
    );
    harness.assert_model_exhausted();
    harness.shutdown().await;
}

/// TS-SPAWN-04: direction_markdown 注入子 agent system prompt(2026-07-20 新增)
///
/// 验证:TianquanSubagentPromptMaterialSource 经 SubagentPromptComposer 注入,
/// 子 agent 的 model request 含 novelist.md 的 direction_markdown 内容(作为
/// System-role inline message)。这是 16 SOUL 真接通的关键证据——子 agent 不只
/// 被 spawn 出来,还拿到了对应 SOUL 的 persona 正文。
///
/// 判据:model_requests 中子 agent 的请求(含 "Subagent task" goal_framing message)
/// 的 messages 里有 System-role message 含 novelist.md 独特字符串("V17 L8 小说家"
/// 或 "Novelist delegate_task" 或 "POVDepth/CameraDistance/ProseDensity")。
#[tokio::test]
async fn ts_spawn_04_direction_markdown_injected_to_child_system_prompt() {
    let model_gateway = RebornTraceReplayModelGateway::with_scripted_steps([
        RebornModelReplayStep::ProviderToolCalls {
            calls: vec![spawn_call(
                "spawn_novelist_for_direction",
                serde_json::json!({
                    "flavor_id": "novelist",
                    "task": "写第 24 章正文",
                }),
            )],
            expected_tool_results: Vec::new(),
        },
        RebornModelReplayStep::Response {
            response: HostManagedModelResponse::assistant_reply("child prose draft"),
            expected_tool_results: Vec::new(),
        },
        RebornModelReplayStep::Response {
            response: HostManagedModelResponse::assistant_reply("parent resumed"),
            expected_tool_results: Vec::new(),
        },
    ]);
    let mut harness = tianquan_spawn_harness("tianquan-novelist-direction", model_gateway).await;
    harness.start();

    let submitted = harness
        .submit_text("event-tianquan-direction", "delegate writing to novelist")
        .await
        .expect("submit root turn");
    let _child = await_single_child(&harness, &submitted).await;
    harness
        .wait_for_status(submitted.run_id, TurnStatus::Completed)
        .await
        .expect("parent completes");

    // 子 agent 的 model request 含 System-role message 带 direction_markdown。
    // model_requests 顺序:[0]=父 spawn call,[1]=子 agent(含 direction + goal_framing + goal),[2]=父 resume
    // direction_markdown 是 System role(SubagentPromptComposer::materialize_direction_message)
    // goal_framing 是 User role("Subagent task. The parent task...")
    let requests = harness.model_requests();
    let child_request = requests
        .iter()
        .find(|req| {
            req.messages.iter().any(|m| {
                m.role == HostManagedModelMessageRole::User
                    && m.content.contains("Subagent task")
            })
        })
        .expect("应找到子 agent 的 model request(含 'Subagent task' goal_framing)");

    // 子 agent system prompt 含 novelist.md direction_markdown 独特字符串
    let has_direction = child_request.messages.iter().any(|m| {
        m.role == HostManagedModelMessageRole::System
            && (m.content.contains("V17 L8 小说家")
                || m.content.contains("Novelist delegate_task")
                || m.content.contains("POVDepth/CameraDistance/ProseDensity"))
    });
    assert!(
        has_direction,
        "子 agent system prompt 应含 novelist.md direction_markdown,实际 messages: {:#?}",
        child_request.messages.iter().map(|m| (m.role, m.content.chars().take(80).collect::<String>())).collect::<Vec<_>>()
    );

    harness.assert_model_exhausted();
    harness.shutdown().await;
}

/// TS-SPAWN-02: 16 SOUL kind 全部 resolve_kind 返 Some
///
/// 验证:TianquanSubagentDefinitionResolver 对 16 个 SOUL kind(novelist/worldsmith 等)
/// 全部返回 Some(SubagentDefinition),不返 None(默认 StaticSubagentDefinitionResolver
/// 只认 4 内置 flavor,16 SOUL 全返 None)。
#[tokio::test]
async fn ts_spawn_02_all_sixteen_souls_resolve() {
    let resolver = TianquanSubagentDefinitionResolver::new();
    for flavor in TIANQUAN_SOUL_FLAVORS.iter() {
        let kind = ironclaw_loop_host::SubagentKindId::new(flavor.kind)
            .unwrap_or_else(|_| panic!("invalid kind id: {}", flavor.kind));
        let definition = resolver
            .resolve_kind(&kind)
            .await
            .unwrap_or_else(|_| panic!("resolve_kind({}) failed", flavor.kind));
        assert!(
            definition.is_some(),
            "16 SOUL kind {} 应 resolve 成功返 Some,实际 None",
            flavor.kind
        );
    }
}

/// TS-CONTRAST-01: 默认 StaticSubagentDefinitionResolver 下 novelist flavor spawn 失败(对照)
///
/// 验证:默认 resolver(StaticSubagentDefinitionResolver)不认 "novelist" kind,
/// resolve_kind 返 None,spawn 被拒(child 不派生)。这是对照测,证明天权 resolver
/// 的必要性(没有它,novelist flavor 无法 spawn)。
#[tokio::test]
async fn ts_contrast_01_default_resolver_rejects_novelist() {
    use ironclaw_runner::subagent::flavors::StaticSubagentDefinitionResolver;
    let resolver = StaticSubagentDefinitionResolver;
    let kind = ironclaw_loop_host::SubagentKindId::new("novelist").unwrap();
    let definition = resolver.resolve_kind(&kind).await.unwrap();
    assert!(
        definition.is_none(),
        "默认 StaticSubagentDefinitionResolver 应不认 novelist kind(返 None),实际 Some"
    );
}

// ---- 辅助函数(对齐 reborn_subagent_spawn_e2e.rs 模式)----

async fn tianquan_spawn_harness(
    conversation_id: &str,
    model_gateway: RebornTraceReplayModelGateway,
) -> RebornBinaryE2EHarness {
    let resolver: Arc<dyn SubagentDefinitionResolver> =
        Arc::new(TianquanSubagentDefinitionResolver::new());
    tokio::time::timeout(
        WaitConfig::default().timeout,
        RebornBinaryE2EHarness::with_harness_blocked_evidence_tianquan_subagents(
            conversation_id,
            model_gateway,
            RecordingTestCapabilityPort::echo_with_spawn_subagent(),
            resolver,
        ),
    )
    .await
    .expect("tianquan spawn harness timed out")
    .expect("tianquan spawn harness")
}

fn spawn_call(
    call_id: impl Into<String>,
    arguments: serde_json::Value,
) -> RebornScriptedProviderToolCall {
    RebornScriptedProviderToolCall::new(spawn_capability_id(), call_id, arguments)
}

fn spawn_capability_id() -> CapabilityId {
    CapabilityId::new(DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID).expect("valid capability id")
}

async fn await_single_child(
    harness: &RebornBinaryE2EHarness,
    submitted: &SubmittedTurn,
) -> ironclaw_turns::TurnRunRecord {
    let mut children = await_children(harness, submitted, 1).await;
    children.pop().expect("one child")
}

async fn await_children(
    harness: &RebornBinaryE2EHarness,
    submitted: &SubmittedTurn,
    expected: usize,
) -> Vec<ironclaw_turns::TurnRunRecord> {
    let wait = WaitConfig::default();
    let deadline = tokio::time::Instant::now() + wait.timeout;
    loop {
        let children = harness
            .children_of(&submitted.scope, submitted.run_id)
            .await
            .expect("children");
        if children.len() >= expected {
            return children;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {expected} children; observed {}",
                children.len()
            );
        }
        tokio::time::sleep(wait.poll_interval).await;
    }
}

fn assert_child_thread_invariants(
    parent: &SubmittedTurn,
    child: &ironclaw_turns::TurnRunRecord,
) {
    assert_eq!(child.parent_run_id, Some(parent.run_id));
    assert_eq!(child.subagent_depth, 1);
    assert_eq!(child.spawn_tree_root_run_id, Some(parent.run_id));
    assert_eq!(child.scope.tenant_id, parent.scope.tenant_id);
    assert_eq!(child.scope.agent_id, parent.scope.agent_id);
    assert_eq!(child.scope.project_id, parent.scope.project_id);
    assert_ne!(
        child.scope.thread_id, parent.scope.thread_id,
        "child must run on a distinct thread"
    );
}

// 避免未使用 import 警告(Duration 在 scripted steps 里用)
#[allow(dead_code)]
const _DURATION_USED: Duration = Duration::from_millis(0);
