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
        AgentLoopHostError, AgentLoopHostErrorKind, sanitize_model_visible_text, LoopPromptBundle,
        LoopPromptBundleRequest, LoopPromptPort, LoopRunContext, PromptMode,
    },
};
use tianquan_subagents::TianquanSubagentPromptMaterialSource;

fn public_task() -> String {
    let mut task = String::from("Public world-only candidate task. ");
    while task.len() < 12 * 1024 {
        task.push_str("Use only public fictional town water-allocation facts and return candidates. ");
    }
    task.truncate(12 * 1024); // Entire public fixture is ASCII.
    task
}

fn scope(context: &LoopRunContext) -> ThreadScope {
    ThreadScope {
        tenant_id: context.scope.tenant_id.clone(),
        agent_id: context.scope.agent_id.clone().expect("public agent"),
        project_id: context.scope.project_id.clone(),
        owner_user_id: context.actor.as_ref().map(|actor| actor.user_id.clone()),
        mission_id: None,
    }
}

async fn real_worldsmith_source() -> (LoopRunContext, LoopRunContext, Arc<dyn SubagentPromptMaterialSource>) {
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
        parent_run_context: parent.clone(),
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
            content: MessageContent::text(public_task()),
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
        parent,
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


use serde_json::Value;
fn public_catalogs() -> (Value, Value) {
    let expected_path = std::env::var("TIANQUAN_APPROVED_METHOD_EXPECTED_PATH")
        .expect("configure approved public fixture path");
    let registry_path = std::env::var("TIANQUAN_ROLE_METHOD_REGISTRY_PATH")
        .expect("configure public registry path");
    let expected_bytes = std::fs::read(expected_path).expect("read approved public fixture");
    let registry_bytes = std::fs::read(registry_path).expect("read public registry");
    // Root's driver verifies whole public-file SHA before launching this test.
    // This warm package has no direct cryptographic dependency; no source-pin
    // qualification is invented here from operator environment or descriptors.
    let expected: Value = serde_json::from_slice(&expected_bytes).expect("public expected JSON");
    let registry: Value = serde_json::from_slice(&registry_bytes).expect("public registry JSON");
    assert!(expected["sourceCatalogRawSha256"] == registry["contentCatalogRawSha256"]);
    (expected, registry)
}
fn checked_public_role<'a>(expected: &'a Value, registry: &'a Value, role: &str) -> &'a Value {
    let entry = expected["roles"].as_array().expect("roles").iter()
        .find(|entry| entry["role"].as_str() == Some(role)).expect("expected public role");
    let registered = registry["baseMethods"].as_array().expect("baseMethods").iter()
        .find(|entry| entry["role"].as_str() == Some(role)).expect("registered public role");
    let raw = entry["rawBody"].as_str().expect("public raw body");
    let _normalized = entry["normalizedBody"].as_str().expect("public normalized body");
    // Descriptor consistency only. Required body's SHA is checked by the real
    // HostCheckedOpaqueInputPort; pure sanitizer case does not sign hashes.
    assert!(registered["body"].as_str() == Some(raw), "public raw registry body mismatch");
    assert!(registered["rawBodySha256"] == entry["rawSha256"], "public raw registry SHA mismatch");
    assert!(registered["normalizedBodySha256"] == entry["normalizedSha256"], "public normalized registry SHA mismatch");
    entry
}
// Classify only changed tokens; never report token values or body snippets.
// Actual transformation is always the production sanitize_model_visible_text.
fn changed_token_classes(body: &str) -> [usize; 6] {
    let mut counts = [0; 6];
    for token in body.split_whitespace() {
        if sanitize_model_visible_text(token) == token { continue; }
        let normalized = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_').to_ascii_lowercase();
        let class = if normalized.starts_with("sk-") { 0 }
        else if normalized.contains("api_key") { 1 }
        else if normalized.contains("access_token") { 2 }
        else if normalized.contains("raw_credential_sentinel") { 3 }
        else if normalized.contains("raw_provider_secret") { 4 }
        else { 5 };
        counts[class] += 1;
    }
    counts
}
struct PublicRequired {
    parent: LoopRunContext,
    child: LoopRunContext,
    body: String,
    approved_hash: String,
    calls: Mutex<usize>,
}
#[async_trait]
impl OpaquePromptInputBackend for PublicRequired {
    async fn persist_before_submit(
        &self, _: &HostSpawnInputFacts<'_>,
    ) -> Result<OpaquePromptInputOutcome<OpaquePromptInputRecord>, AgentLoopHostError> {
        panic!("prompt-only public test must not bind or write");
    }
    async fn load_for_child(
        &self, child: &LoopRunContext,
    ) -> Result<OpaquePromptInputOutcome<OpaquePromptInputRecoveredMaterial>, AgentLoopHostError> {
        *self.calls.lock().expect("calls") += 1;
        assert!(child == &self.child, "actual checked child scope/context changed");
        Ok(OpaquePromptInputOutcome::Required(OpaquePromptInputRecoveredMaterial {
            host_parent_context: self.parent.clone(),
            host_child_context: self.child.clone(),
            host_registered_kind: "worldsmith".into(),
            record: OpaquePromptInputRecord {
                parent_context: self.parent.clone(),
                child_run_id: self.child.run_id,
                child_thread_id: self.child.thread_id.clone(),
                child_scope: self.child.scope.clone(),
                registered_kind: "worldsmith".into(),
                binding_ref: "binding:public-approved-worldsmith".into(),
                material_ref: "material:public-approved-worldsmith/1.0.0".into(),
                content_hash: self.approved_hash.clone(), // Original approved source hash, never repaired.
                source_version: "role-method/1.0.0-public-fixture".into(),
            },
            body: self.body.clone(),
        }))
    }
}
#[tokio::test]
async fn world32_public02_worldsmith_required_full_body_12k_goal_reaches_inner() {
    let (expected, registry) = public_catalogs();
    let entry = checked_public_role(&expected, &registry, "worldsmith");
    let body = entry["normalizedBody"].as_str().expect("normalized public body").to_owned();
    let approved_hash = entry["normalizedSha256"].as_str().expect("approved SHA").to_owned();
    assert_eq!(body.len(), 14353);
    let changed = sanitize_model_visible_text(body.clone()) != body;
    let classes = changed_token_classes(&body);
    println!("{}", serde_json::json!({"role":"worldsmith","approvedRawSha256":entry["rawSha256"],
        "approvedNormalizedSha256":approved_hash,"bodyBytes":body.len(),"sanitizerChanged":changed,
        "changedTokenClassCounts":classes,"publicTaskBytes":public_task().len()}));
    let (child, parent, original) = real_worldsmith_source().await;
    let backend = Arc::new(PublicRequired { parent, child: child.clone(), body: body.clone(),
        approved_hash, calls: Mutex::new(0) });
    let source: Arc<dyn SubagentPromptMaterialSource> =
        Arc::new(OpaqueBackedSubagentPromptMaterialSource::new(original, backend.clone()));
    // Actual checked port charges full record/body; actual composer keeps the default 64 KiB limit.
    let composer = SubagentPromptComposer::new(source.clone())
        .with_opaque_input_port(source.opaque_input_port().expect("actual checked port"));
    let inner = Arc::new(ControlledInner::default());
    let prompt = SubagentLoopPromptPort::new(inner.clone(), child, composer);
    let error = match prompt.build_prompt_bundle(request()).await {
        Err(error) => error,
        Ok(_) => panic!("controlled inner must fail after full Required render"),
    };
    assert_eq!(*backend.calls.lock().expect("calls"), 1);
    let captured = inner.requests.lock().expect("requests");
    println!("{}", serde_json::json!({"role":"worldsmith","innerCallCount":captured.len(),
        "controlledInnerReached":error.safe_summary == "controlled-public-stage=inner"}));
    assert!(!changed, "approved worldsmith normalized body changes under actual model redactor; see safe class counts");
    assert!(error.safe_summary == "controlled-public-stage=inner",
        "actual Required full-body composer failed before controlled inner");
    assert_eq!(captured.len(), 1);
    let messages = &captured[0].inline_messages;
    assert_eq!(messages.len(), 4);
    assert!(messages[0].safe_body.as_str().contains("kind=worldsmith"));
    assert!(messages[1].safe_body.as_str().contains("Subagent task."));
    assert!(messages[2].safe_body.as_str() == format!("Subagent task:\n{}", public_task()));
    assert!(messages[3].safe_body.as_str() == body, "full original approved body must remain byte-equivalent");
    assert!(messages.iter().map(|m| m.safe_body.as_str().len()).sum::<usize>() < 64 * 1024);
}
#[test]
fn world32_public02_all16_approved_normalized_bodies_sanitize_unchanged() {
    let (expected, registry) = public_catalogs();
    let roles = expected["roles"].as_array().expect("roles");
    assert_eq!(roles.len(), 16);
    assert_eq!(expected["roleCount"], 16);
    let mut changed_roles = 0;
    for role in roles {
        let name = role["role"].as_str().expect("public role");
        let entry = checked_public_role(&expected, &registry, name);
        let body = entry["normalizedBody"].as_str().expect("public normalized body");
        let changed = sanitize_model_visible_text(body) != body;
        changed_roles += usize::from(changed);
        println!("{}", serde_json::json!({"role":name,"approvedRawSha256":entry["rawSha256"],
            "approvedNormalizedSha256":entry["normalizedSha256"],"normalizedBytes":body.len(),
            "sanitizerChanged":changed,"changedTokenClassCounts":changed_token_classes(body)}));
    }
    assert_eq!(changed_roles, 0, "approved normalized role bodies changed under actual production sanitizer; see safe role/class metadata");
}

