//! 有限真实caller控制：实际spawn port→checked adapter→实际child prompt port。
//! backend仅内存控制，不冒实际TQ网络/PG持久化/provider验收。
use super::*;
use crate::{
    HostSpawnInputFacts, OpaqueBackedSubagentPromptMaterialSource, OpaquePromptInputBackend,
    OpaquePromptInputOutcome, OpaquePromptInputRecord, OpaquePromptInputRecoveredMaterial,
    SubagentLoopPromptPort, SubagentPromptComposer, SubagentPromptGoal, SubagentPromptMaterial,
    SubagentPromptMaterialSource,
};
use ironclaw_turns::run_profile::{
    LoopPromptBundle, LoopPromptBundleRequest, LoopPromptPort, PromptMode,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const BODY: &str =
    "完整方法正文\n第一章：保持角色动机。\n第二章：检验因果与叙事约束。\n末章：保留审校步骤。";
#[derive(Clone, Copy)]
enum Mode {
    Good,
    PersistFailure,
    Missing,
    CrossScope,
    WrongHash,
    NotRequired,
}
struct Stored {
    parent: LoopRunContext,
    run: TurnRunId,
    thread: ThreadId,
    scope: TurnScope,
    role: String,
}
struct Backend {
    mode: Mode,
    stored: std::sync::Mutex<Option<Stored>>,
    events: std::sync::Mutex<Vec<&'static str>>,
    child_runs: Arc<RecordingChildRuns>,
    goal_store: Arc<RecordingGoalStore>,
}
impl Backend {
    fn record(&self, s: &Stored) -> OpaquePromptInputRecord {
        OpaquePromptInputRecord {
            parent_context: s.parent.clone(),
            child_run_id: s.run,
            child_thread_id: s.thread.clone(),
            child_scope: s.scope.clone(),
            registered_kind: s.role.clone(),
            binding_ref: "binding:caller-1".into(),
            material_ref: "material:caller-1".into(),
            content_hash: hex::encode(Sha256::digest(BODY.as_bytes())),
            source_version: "methods/1".into(),
        }
    }
}
#[async_trait]
impl OpaquePromptInputBackend for Backend {
    async fn persist_before_submit(
        &self,
        f: &HostSpawnInputFacts<'_>,
    ) -> Result<OpaquePromptInputOutcome<OpaquePromptInputRecord>, AgentLoopHostError> {
        // 下游原goal/submit此时必须未发生。不是仅断言调用次数。
        assert!(self.child_runs.requests().is_empty());
        assert!(self.goal_store.puts().is_empty());
        self.events.lock().unwrap().push("persist");
        if matches!(self.mode, Mode::PersistFailure) {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unavailable,
                "controlled persistence failure",
            ));
        }
        if matches!(self.mode, Mode::NotRequired) {
            return Ok(OpaquePromptInputOutcome::NotRequired);
        }
        let s = Stored {
            parent: f.parent_context.clone(),
            run: f.child_run_id,
            thread: f.child_thread_id.clone(),
            scope: TurnScope::new_with_owner(
                f.parent_context.scope.tenant_id.clone(),
                f.parent_context.scope.agent_id.clone(),
                f.parent_context.scope.project_id.clone(),
                f.child_thread_id.clone(),
                f.parent_context.scope.explicit_owner_user_id().cloned(),
            ),
            role: f.registered_kind.into(),
        };
        let r = self.record(&s);
        *self.stored.lock().unwrap() = Some(s);
        Ok(OpaquePromptInputOutcome::Required(r))
    }
    async fn load_for_child(
        &self,
        c: &LoopRunContext,
    ) -> Result<OpaquePromptInputOutcome<OpaquePromptInputRecoveredMaterial>, AgentLoopHostError>
    {
        self.events.lock().unwrap().push("load");
        if matches!(self.mode, Mode::NotRequired) {
            return Ok(OpaquePromptInputOutcome::NotRequired);
        }
        if matches!(self.mode, Mode::Missing) {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unavailable,
                "required record missing",
            ));
        }
        let guard = self.stored.lock().unwrap();
        let s = guard.as_ref().expect("persisted before load");
        // 用真正submit请求登记的child身份恢复context，不能采用caller传来的错scope。
        let request = self
            .child_runs
            .requests()
            .into_iter()
            .next()
            .expect("actual child submit");
        let mut host = c.clone();
        host.run_id = request.requested_run_id.unwrap();
        host.scope = request.child_scope.clone();
        host.thread_id = host.scope.thread_id.clone();
        let mut r = self.record(s);
        if matches!(self.mode, Mode::CrossScope) {
            r.child_scope.tenant_id = TenantId::new("other-tenant").unwrap();
        }
        if matches!(self.mode, Mode::WrongHash) {
            r.content_hash = "0".repeat(64);
        }
        Ok(OpaquePromptInputOutcome::Required(
            OpaquePromptInputRecoveredMaterial {
                host_parent_context: s.parent.clone(),
                host_child_context: host,
                host_registered_kind: s.role.clone(),
                record: r,
                body: BODY.into(),
            },
        ))
    }
}
struct OriginalSource;
#[async_trait]
impl SubagentPromptMaterialSource for OriginalSource {
    async fn material_for_run(
        &self,
        _: &LoopRunContext,
    ) -> Result<SubagentPromptMaterial, AgentLoopHostError> {
        Ok(SubagentPromptMaterial {
            direction_markdown: "原角色方向".into(),
            goal: SubagentPromptGoal {
                task: "原任务".into(),
                handoff: Some("原交接".into()),
            },
            allowed_capabilities: BTreeSet::new(),
        })
    }
}
struct Capture {
    requests: std::sync::Mutex<Vec<LoopPromptBundleRequest>>,
    backend: Arc<Backend>,
}
#[async_trait]
impl LoopPromptPort for Capture {
    async fn build_prompt_bundle(
        &self,
        r: LoopPromptBundleRequest,
    ) -> Result<LoopPromptBundle, AgentLoopHostError> {
        self.backend.events.lock().unwrap().push("inner");
        self.requests.lock().unwrap().push(r);
        // 在真正inner边界捕获；没有假造provider成功。
        Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::Unavailable,
            "captured at real inner prompt boundary",
        ))
    }
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
async fn fixture(
    mode: Mode,
) -> (
    SubagentSpawnCapabilityPort,
    Arc<Backend>,
    Arc<dyn SubagentPromptMaterialSource>,
) {
    let parent = test_run_context_with_explicit_owner_scope("opaque-caller").await;
    let f = spawn_test_port_with_codec_and_recorders(
        parent,
        Arc::new(StaticSpawnInputCodec {
            args: default_spawn_args(),
        }),
    );
    let backend = Arc::new(Backend {
        mode,
        stored: std::sync::Mutex::new(None),
        events: std::sync::Mutex::new(Vec::new()),
        child_runs: f.child_runs.clone(),
        goal_store: f.goal_store.clone(),
    });
    let source: Arc<dyn SubagentPromptMaterialSource> = Arc::new(
        OpaqueBackedSubagentPromptMaterialSource::new(Arc::new(OriginalSource), backend.clone()),
    );
    let port = f
        .port
        .with_opaque_input_port(source.opaque_input_port().unwrap());
    (port, backend, source)
}
async fn child_after_spawn(port: &SubagentSpawnCapabilityPort, b: &Backend) -> LoopRunContext {
    let outcome = invoke_spawn(port).await;
    assert!(
        matches!(
            outcome,
            Resolution::Suspended(Suspension::DependentRun { .. })
        ),
        "actual spawn must dispatch: {outcome:?}"
    );
    let req = b.child_runs.requests();
    assert_eq!(req.len(), 1);
    let s = b.stored.lock().unwrap();
    let stored = s.as_ref();
    if let Some(stored) = stored {
        assert_eq!(req[0].requested_run_id, Some(stored.run));
        assert_eq!(req[0].child_scope, stored.scope);
        assert_eq!(req[0].parent_scope, stored.parent.scope);
    }
    let resolved = InMemoryRunProfileResolver::default()
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .unwrap();
    LoopRunContext::new(
        req[0].child_scope.clone(),
        TurnId::new(),
        req[0].requested_run_id.unwrap(),
        resolved,
    )
}
async fn prompt(
    child: LoopRunContext,
    source: Arc<dyn SubagentPromptMaterialSource>,
    b: Arc<Backend>,
) -> Arc<Capture> {
    let inner = Arc::new(Capture {
        requests: std::sync::Mutex::new(Vec::new()),
        backend: b,
    });
    let composer = SubagentPromptComposer::new(source.clone())
        .with_opaque_input_port(source.opaque_input_port().unwrap());
    let p = SubagentLoopPromptPort::new(inner.clone(), child, composer);
    assert!(p.build_prompt_bundle(request()).await.is_err());
    inner
}
#[tokio::test]
async fn opaque_caller_same_port_precedes_real_submit_and_preserves_full_body_bytes() {
    let (port, b, source) = fixture(Mode::Good).await;
    assert!(Arc::ptr_eq(
        &source.opaque_input_port().unwrap(),
        &source.opaque_input_port().unwrap()
    ));
    let child = child_after_spawn(&port, &b).await;
    let captured = prompt(child, source, b.clone()).await;
    let requests = captured.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let bodies: Vec<_> = requests[0]
        .inline_messages
        .iter()
        .map(|x| x.safe_body.as_str())
        .collect();
    assert_eq!(bodies.iter().filter(|x| **x == BODY).count(), 1);
    assert_eq!(bodies[0], "原角色方向");
    assert!(
        bodies
            .iter()
            .any(|x| x.contains("原任务") && x.contains("原交接"))
    );
    assert_eq!(*b.events.lock().unwrap(), vec!["persist", "load", "inner"]);
}
#[tokio::test]
async fn opaque_caller_persist_failure_blocks_real_goal_and_submit() {
    let (port, b, _) = fixture(Mode::PersistFailure).await;
    let result = port
        .invoke_capability(invocation_for_activity(
            DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID,
            port.test_spawn_authorization(&input_ref()).unwrap(),
        ))
        .await;
    assert!(result.is_err());
    assert!(b.child_runs.requests().is_empty());
    assert!(b.goal_store.puts().is_empty());
    assert_eq!(*b.events.lock().unwrap(), vec!["persist"]);
}
#[tokio::test]
async fn opaque_caller_missing_required_does_not_reach_inner_prompt() {
    let (p, b, s) = fixture(Mode::Missing).await;
    let c = child_after_spawn(&p, &b).await;
    let inner = prompt(c, s, b.clone()).await;
    assert!(inner.requests.lock().unwrap().is_empty());
    assert_eq!(*b.events.lock().unwrap(), vec!["persist", "load"]);
}
#[tokio::test]
async fn opaque_caller_cross_scope_record_does_not_reach_inner_prompt() {
    let (p, b, s) = fixture(Mode::CrossScope).await;
    let c = child_after_spawn(&p, &b).await;
    let inner = prompt(c, s, b.clone()).await;
    assert!(inner.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn opaque_caller_wrong_body_hash_does_not_reach_inner_prompt() {
    let (p, b, s) = fixture(Mode::WrongHash).await;
    let c = child_after_spawn(&p, &b).await;
    let inner = prompt(c, s, b.clone()).await;
    assert!(inner.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn opaque_caller_wrong_actual_child_is_rejected_before_inner_prompt() {
    let (p, b, s) = fixture(Mode::Good).await;
    let mut c = child_after_spawn(&p, &b).await;
    c.run_id = TurnRunId::new();
    let inner = prompt(c, s, b.clone()).await;
    assert!(inner.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn opaque_caller_explicit_not_required_is_byte_equivalent_to_old_none() {
    let (p, b, s) = fixture(Mode::NotRequired).await;
    let c = child_after_spawn(&p, &b).await;
    let old = SubagentPromptComposer::new(Arc::new(OriginalSource))
        .compose(&c, request())
        .await
        .unwrap();
    let new = SubagentPromptComposer::new(s.clone())
        .with_opaque_input_port(s.opaque_input_port().unwrap())
        .compose(&c, request())
        .await
        .unwrap();
    assert_eq!(old, new);
    assert_eq!(*b.events.lock().unwrap(), vec!["persist", "load"]);
}
#[tokio::test]
async fn opaque_caller_whole_material_budget_rejects_without_inner_dispatch() {
    let (p, b, _) = fixture(Mode::Good).await;
    let c = child_after_spawn(&p, &b).await;
    let s: Arc<dyn SubagentPromptMaterialSource> = Arc::new(
        OpaqueBackedSubagentPromptMaterialSource::with_max_material_bytes(
            Arc::new(OriginalSource),
            b.clone(),
            BODY.len() - 1,
        ),
    );
    let inner = prompt(c, s, b).await;
    assert!(inner.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn opaque_factory_initializes_once_with_original_host_arc_identity() {
    use crate::{
        FactoryOpaquePromptInputBackend, OpaquePromptInputBackendFactory,
        OpaquePromptInputHostServices,
    };
    struct Factory {
        expected: OpaquePromptInputHostServices,
        backend: Arc<Backend>,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl OpaquePromptInputBackendFactory for Factory {
        async fn create(
            &self,
            services: OpaquePromptInputHostServices,
        ) -> Result<Arc<dyn OpaquePromptInputBackend>, AgentLoopHostError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(Arc::ptr_eq(&services.turns, &self.expected.turns));
            assert!(Arc::ptr_eq(&services.threads, &self.expected.threads));
            assert!(Arc::ptr_eq(
                &services.definitions,
                &self.expected.definitions
            ));
            Ok(self.backend.clone())
        }
    }
    let parent = test_run_context_with_explicit_owner_scope("opaque-factory-caller").await;
    let f = spawn_test_port_with_codec_and_recorders(
        parent,
        Arc::new(StaticSpawnInputCodec {
            args: default_spawn_args(),
        }),
    );
    let backend = Arc::new(Backend {
        mode: Mode::Good,
        stored: std::sync::Mutex::new(None),
        events: std::sync::Mutex::new(Vec::new()),
        child_runs: f.child_runs.clone(),
        goal_store: f.goal_store.clone(),
    });
    let services = OpaquePromptInputHostServices {
        turns: f.port.deps.turn_state_store.clone(),
        threads: f.port.deps.thread_service.clone(),
        definitions: f.port.deps.definition_resolver.clone(),
    };
    let factory = Arc::new(Factory {
        expected: services.clone(),
        backend: backend.clone(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let proxy = Arc::new(FactoryOpaquePromptInputBackend::new(factory.clone()));
    assert!(
        proxy
            .initialize_host_services(services.clone())
            .await
            .is_ok()
    );
    assert!(proxy.initialize_host_services(services).await.is_err());
    assert_eq!(factory.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let source: Arc<dyn SubagentPromptMaterialSource> = Arc::new(
        OpaqueBackedSubagentPromptMaterialSource::new(Arc::new(OriginalSource), proxy),
    );
    let port = f
        .port
        .with_opaque_input_port(source.opaque_input_port().unwrap());
    let _child = child_after_spawn(&port, &backend).await;
    assert_eq!(factory.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    // 此测试只真实port/mechanism；不把内存Backend当TQPG/provider发行。
}
