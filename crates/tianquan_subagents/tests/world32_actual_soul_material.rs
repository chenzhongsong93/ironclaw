//! 外部 test-only 候选：真实天权 source/SOUL，公开任务与 user-only/goal-miss。
//! 内存 thread/goal 和控制 opaque/inner 不能替代真实 PG/HTTP/provider 验收。
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_agent_loop::test_support::test_run_context;
use ironclaw_host_api::{AgentId, ProjectId, UserId};
use ironclaw_loop_host::{
    HostSpawnInputFacts, OpaqueBackedSubagentPromptMaterialSource, OpaquePromptInputBackend,
    OpaquePromptInputOutcome, OpaquePromptInputRecord, OpaquePromptInputRecoveredMaterial,
    SpawnSubagentMode, SubagentKindId, SubagentLoopPromptPort, SubagentPromptComposer,
    SubagentPromptMaterialSource, SubagentThreadKind, SubagentThreadMetadata,
};
use ironclaw_runner::subagent::goal_store::{
    InMemoryBoundedSubagentGoalStore, SubagentGoalStore, SubagentGoalStoreError,
};
use ironclaw_threads::{
    AcceptInboundMessageRequest, EnsureThreadRequest, InMemorySessionThreadService, MessageContent,
    SessionThreadService, ThreadHistoryRequest, ThreadScope,
};
use ironclaw_turns::{
    GateRef, LoopResultRef, TurnScope,
    run_profile::{
        AgentLoopHostError, AgentLoopHostErrorKind, HostSafeDiagnostic, LoopPromptBundle,
        LoopPromptBundleRequest, LoopPromptPort, LoopRunContext, PromptMode,
    },
};
use tianquan_subagents::TianquanSubagentPromptMaterialSource;

const PUBLIC_TASK: &str =
    "为公开测试世界建立候选：城镇居民遵守共同的水资源分配规则；仅产出 world.only 候选。";

fn scope(context: &LoopRunContext) -> ThreadScope {
    ThreadScope {
        tenant_id: context.scope.tenant_id.clone(),
        agent_id: context.scope.agent_id.clone().expect("public agent"),
        project_id: context.scope.project_id.clone(),
        owner_user_id: context.actor.as_ref().map(|actor| actor.user_id.clone()),
        mission_id: None,
    }
}

async fn real_worldsmith_source() -> (LoopRunContext, Arc<dyn SubagentPromptMaterialSource>) {
    assert!(
        std::env::var_os("TIANQUAN_SOUL_BUNDLE_DIR").is_some(),
        "configure the original verified public SOUL bundle before this external test"
    );
    let mut child = test_run_context("world32-public-material");
    let owner = UserId::new("public-owner").expect("public owner");
    child.scope = TurnScope::new_with_owner(
        child.scope.tenant_id.clone(),
        Some(AgentId::new("public-novel-agent").expect("public agent")),
        Some(ProjectId::new("public-world-project").expect("public project")),
        child.thread_id.clone(),
        Some(owner.clone()),
    );
    child.actor = Some(
        serde_json::from_value(serde_json::json!({"user_id": owner.as_str()}))
            .expect("public actor shape user_id"),
    );
    let mut parent = test_run_context("world32-public-parent");
    parent.scope = TurnScope::new_with_owner(
        child.scope.tenant_id.clone(),
        child.scope.agent_id.clone(),
        child.scope.project_id.clone(),
        parent.thread_id.clone(),
        Some(owner),
    );
    parent.actor = child.actor.clone();
    let metadata = SubagentThreadMetadata {
        kind: SubagentThreadKind::Subagent,
        parent_run_id: parent.run_id,
        parent_thread_id: parent.thread_id.clone(),
        tree_root_run_id: parent.run_id,
        child_run_id: child.run_id,
        subagent_kind: SubagentKindId::new("worldsmith").expect("worldsmith"),
        mode: SpawnSubagentMode::Blocking,
        result_ref: LoopResultRef::new("result:public-worldsmith").expect("public result"),
        handoff: Some("公开测试交接，不作为 goal store 缺失时的正文来源。".into()),
        parent_run_context: parent,
        gate_ref: GateRef::new("gate:public-worldsmith").expect("public gate"),
    };
    let metadata_value = serde_json::to_value(&metadata).expect("typed metadata");
    assert_eq!(metadata_value["flavor"], "worldsmith");
    assert_eq!(metadata_value["kind"], "subagent");
    assert_eq!(metadata_value["mode"], "blocking");
    assert!(metadata_value.get("subagent_kind").is_none());
    let threads = Arc::new(InMemorySessionThreadService::default());
    threads
        .ensure_thread(EnsureThreadRequest {
            scope: scope(&child),
            thread_id: Some(child.thread_id.clone()),
            created_by_actor_id: "public-test-host".into(),
            title: None,
            metadata_json: Some(metadata_value.to_string()),
        })
        .await
        .expect("stage=thread_metadata");
    let accepted = threads
        .accept_inbound_message(AcceptInboundMessageRequest {
            scope: scope(&child),
            thread_id: child.thread_id.clone(),
            actor_id: "public-owner".into(),
            source_binding_id: Some("public-worldsmith-source".into()),
            reply_target_binding_id: Some("public-worldsmith-reply".into()),
            external_event_id: Some("public-worldsmith-event".into()),
            content: MessageContent::text(PUBLIC_TASK),
        })
        .await
        .expect("stage=first_user");
    threads
        .mark_message_submitted(
            &scope(&child),
            &child.thread_id,
            accepted.message_id,
            child.turn_id.to_string(),
            child.run_id.to_string(),
        )
        .await
        .expect("stage=submitted_user");
    let history = threads
        .list_thread_history(ThreadHistoryRequest {
            scope: scope(&child),
            thread_id: child.thread_id.clone(),
        })
        .await
        .expect("stage=user_only_history");
    assert_eq!(history.messages.len(), 1);
    let goals = Arc::new(InMemoryBoundedSubagentGoalStore::new());
    assert!(matches!(
        goals.get_goal(&child.scope, child.run_id).await,
        Err(SubagentGoalStoreError::NotFound { .. })
    ));
    (
        child,
        Arc::new(TianquanSubagentPromptMaterialSource::new(goals, threads)),
    )
}

fn request() -> LoopPromptBundleRequest {
    LoopPromptBundleRequest {
        mode: PromptMode::TextOnly,
        context_cursor: None,
        surface_version: None,
        capability_view: None,
        checkpoint_state_ref: None,
        max_messages: None,
        inline_messages: Vec::new(),
    }
}

#[derive(Default)]
struct ControlledInner {
    requests: Mutex<Vec<LoopPromptBundleRequest>>,
}
#[async_trait]
impl LoopPromptPort for ControlledInner {
    async fn build_prompt_bundle(
        &self,
        request: LoopPromptBundleRequest,
    ) -> Result<LoopPromptBundle, AgentLoopHostError> {
        self.requests.lock().expect("requests").push(request);
        Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::Unavailable,
            "controlled-public-stage=inner",
        ))
    }
}

struct ControlledOpaque {
    fail: bool,
    load_calls: Mutex<usize>,
}
#[async_trait]
impl OpaquePromptInputBackend for ControlledOpaque {
    async fn persist_before_submit(
        &self,
        _: &HostSpawnInputFacts<'_>,
    ) -> Result<OpaquePromptInputOutcome<OpaquePromptInputRecord>, AgentLoopHostError> {
        panic!("this prompt-only control must not call spawn persist")
    }
    async fn load_for_child(
        &self,
        _: &LoopRunContext,
    ) -> Result<OpaquePromptInputOutcome<OpaquePromptInputRecoveredMaterial>, AgentLoopHostError> {
        *self.load_calls.lock().expect("load calls") += 1;
        if self.fail {
            Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::Invalid,
                "controlled-public-stage=opaque",
            )
            .with_diagnostic_ref(HostSafeDiagnostic::http_status(503).expect("503").diagnostic_ref()))
        } else {
            Ok(OpaquePromptInputOutcome::NotRequired)
        }
    }
}

#[tokio::test]
async fn world32_actual_soul_material_goal_not_found_user_fallback_reaches_inner() {
    let (child, source) = real_worldsmith_source().await;
    let material = source.material_for_run(&child).await.expect("stage=material");
    assert!(material.direction_markdown.contains("SOUL-BUNDLE novel-studio-souls/1.0.1"));
    assert!(material.direction_markdown.contains("kind=worldsmith"));
    assert_eq!(material.goal.task, PUBLIC_TASK);
    assert_eq!(material.goal.handoff, None, "user-only fallback deliberately ignores metadata handoff");
    let inner = Arc::new(ControlledInner::default());
    let prompt = SubagentLoopPromptPort::new(inner.clone(), child, SubagentPromptComposer::new(source));
    let error = match prompt.build_prompt_bundle(request()).await {
        Err(error) => error,
        Ok(_) => panic!("controlled inner must fail after real material/render"),
    };
    assert_eq!(error.safe_summary, "controlled-public-stage=inner");
    let captured = inner.requests.lock().expect("requests");
    assert_eq!(captured.len(), 1);
    let bodies: Vec<_> = captured[0].inline_messages.iter().map(|m| m.safe_body.as_str()).collect();
    assert_eq!(bodies.len(), 3);
    assert!(bodies[0].contains("kind=worldsmith"));
    assert!(bodies[1].contains("Subagent task."));
    assert_eq!(bodies[2], format!("Subagent task:\n{PUBLIC_TASK}"));
}

#[tokio::test]
async fn world32_actual_soul_material_controlled_opaque_failure_stops_before_inner() {
    let (child, original) = real_worldsmith_source().await;
    let backend = Arc::new(ControlledOpaque { fail: true, load_calls: Mutex::new(0) });
    let source: Arc<dyn SubagentPromptMaterialSource> =
        Arc::new(OpaqueBackedSubagentPromptMaterialSource::new(original, backend.clone()));
    let composer = SubagentPromptComposer::new(source.clone())
        .with_opaque_input_port(source.opaque_input_port().expect("actual checked port"));
    let inner = Arc::new(ControlledInner::default());
    let prompt = SubagentLoopPromptPort::new(inner.clone(), child, composer);
    let error = match prompt.build_prompt_bundle(request()).await {
        Err(error) => error,
        Ok(_) => panic!("controlled opaque must fail"),
    };
    assert_eq!(error.safe_summary, "controlled-public-stage=opaque");
    assert_eq!(
        error.diagnostic_ref.as_ref().map(|r| r.as_str()),
        Some("diag:host-safe.http.status-503")
    );
    assert_eq!(*backend.load_calls.lock().expect("load calls"), 1);
    assert!(inner.requests.lock().expect("requests").is_empty());
}

#[tokio::test]
async fn world32_actual_soul_material_explicit_not_required_preserves_inner_request() {
    let (child, original) = real_worldsmith_source().await;
    let expected = SubagentPromptComposer::new(original.clone())
        .compose(&child, request()).await.expect("real original render");
    let backend = Arc::new(ControlledOpaque { fail: false, load_calls: Mutex::new(0) });
    let source: Arc<dyn SubagentPromptMaterialSource> =
        Arc::new(OpaqueBackedSubagentPromptMaterialSource::new(original, backend.clone()));
    let composer = SubagentPromptComposer::new(source.clone())
        .with_opaque_input_port(source.opaque_input_port().expect("actual checked port"));
    let inner = Arc::new(ControlledInner::default());
    let prompt = SubagentLoopPromptPort::new(inner.clone(), child, composer);
    let error = match prompt.build_prompt_bundle(request()).await {
        Err(error) => error,
        Ok(_) => panic!("controlled inner must fail"),
    };
    assert_eq!(error.safe_summary, "controlled-public-stage=inner");
    assert_eq!(*backend.load_calls.lock().expect("load calls"), 1);
    let captured = inner.requests.lock().expect("requests");
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].inline_messages, expected.inline_messages);
    assert_eq!(captured[0].capability_view, expected.capability_view);
}

