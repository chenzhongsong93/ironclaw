use std::sync::Arc;

use ironclaw_agent_loop::test_support::test_run_context;
use ironclaw_host_api::{AgentId, ProjectId, ThreadId};
use ironclaw_loop_host::{
    SpawnSubagentMode, SubagentKindId, SubagentPromptMaterialSource, SubagentThreadKind,
    SubagentThreadMetadata,
};
use ironclaw_runner::subagent::goal_store::{InMemoryBoundedSubagentGoalStore, SubagentGoal};
use ironclaw_threads::{
    EnsureThreadRequest, InMemorySessionThreadService, SessionThreadService, ThreadScope,
};
use ironclaw_turns::{GateRef, LoopResultRef, TurnRunId, run_profile::LoopRunContext};
use tianquan_subagents::TianquanSubagentPromptMaterialSource;

fn thread_scope(context: &LoopRunContext) -> ThreadScope {
    ThreadScope {
        tenant_id: context.scope.tenant_id.clone(),
        agent_id: context.scope.agent_id.clone().expect("agent id").clone(),
        project_id: context.scope.project_id.clone(),
        owner_user_id: context.actor.as_ref().map(|actor| actor.user_id.clone()),
        mission_id: None,
    }
}

#[tokio::test]
#[ignore = "requires TIANQUAN_SOUL_BUNDLE_DIR pointing to the sibling TianQuan worktree"]
async fn material_source_uses_thread_role_and_real_pinned_novelist_bundle() {
    let mut context = test_run_context("novelist-material-source");
    context.scope.agent_id = Some(AgentId::new("novel-agent").expect("agent id"));
    context.scope.project_id = Some(ProjectId::new("project-novel").expect("project id"));

    let metadata = SubagentThreadMetadata {
        kind: SubagentThreadKind::Subagent,
        parent_run_id: TurnRunId::new(),
        parent_thread_id: ThreadId::new("parent-thread").expect("parent thread id"),
        tree_root_run_id: context.run_id,
        child_run_id: context.run_id,
        subagent_kind: SubagentKindId::new("novelist").expect("novelist role"),
        mode: SpawnSubagentMode::Blocking,
        result_ref: LoopResultRef::new("result:novelist-material").expect("result ref"),
        handoff: Some("Preserve the locked chapter context.".into()),
        parent_run_context: context.clone(),
        gate_ref: GateRef::new("gate:novelist-material").expect("gate ref"),
    };
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope(&context),
            thread_id: Some(context.thread_id.clone()),
            created_by_actor_id: "test-host".into(),
            title: None,
            metadata_json: Some(serde_json::to_string(&metadata).expect("metadata JSON")),
        })
        .await
        .expect("create scoped novelist child thread");

    let goal_store = Arc::new(InMemoryBoundedSubagentGoalStore::new());
    goal_store
        .put(
            &context.scope,
            context.run_id,
            SubagentGoal {
                task: "Write the assigned chapter excerpt.".into(),
                handoff: Some("Return only the authored excerpt.".into()),
            },
        )
        .expect("store child goal");

    let source = TianquanSubagentPromptMaterialSource::new(goal_store, thread_service);
    let material = source
        .material_for_run(&context)
        .await
        .expect("resolve role from thread metadata and pinned TianQuan bundle");

    assert!(
        material
            .direction_markdown
            .contains("SOUL-BUNDLE novel-studio-souls/1.0.0")
    );
    assert!(material.direction_markdown.contains("roleSha256="));
    assert!(material.direction_markdown.contains("V17 L8 小说家 Agent"));
    assert!(material.allowed_capabilities.is_empty());
    assert_eq!(material.goal.task, "Write the assigned chapter excerpt.");
    // The accepted goal-store record is the prompt authority; thread metadata
    // handoff is only the fallback/recovery copy.
    assert_eq!(
        material.goal.handoff.as_deref(),
        Some("Return only the authored excerpt.")
    );
}
