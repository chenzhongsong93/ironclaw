//! HFP01–03：真实factory与executor，InMemory依赖/NoModel seam，不授业务资格。
use super::*;
use ironclaw_runner::turn_scheduler::TurnRunExecutor;
use std::io::Write;

const PRIVATE_REASON: &str = "HFP_PRIVATE_REASON_CANARY";

fn executor(
    fixture: &HostFixture,
    claimed: &ClaimedTurnRun,
    factory: Arc<dyn HostFactory>,
) -> (
    RebornTurnRunExecutor,
    Arc<FilesystemTurnStateRowStore<InMemoryBackend>>,
) {
    let store = Arc::new(in_memory_turn_state_store());
    let mut registry = DriverRegistry::new();
    registry
        .register_driver(
            Arc::new(TextOnlyFinalReplyDriver {
                descriptor: claimed.resolved_run_profile.loop_driver.clone(),
            }),
            DriverRequirements::all_optional(),
            DriverKind::Reference,
        )
        .expect("original driver registration");
    (
        RebornTurnRunExecutor::new(
            loop_exit_applier_for_fixture(fixture, store.clone()),
            Arc::new(registry),
            factory,
            None,
        ),
        store,
    )
}

async fn planned_claim(fixture: &HostFixture) -> ClaimedTurnRun {
    let profile = default_planned_run_profile_resolver()
        .expect("registered profile resolver")
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .expect("registered planned profile");
    let mut claimed = fixture.claimed.clone();
    claimed.state.resolved_run_profile_id = profile.profile_id.clone();
    claimed.state.resolved_run_profile_version = profile.profile_version;
    claimed.resolved_run_profile = profile;
    claimed
}

#[tokio::test]
async fn hfp01_actual_factory_valid_control_without_model() {
    let fixture = HostFixture::new("thread-hfp-valid", "public host-only control").await;
    let host = fixture
        .factory()
        .create_host(&fixture.claimed)
        .await
        .expect("legal original factory control");
    assert_eq!(host.run_context().run_id, fixture.context.run_id);
    assert_eq!(host.run_context().scope, fixture.context.scope);
    assert!(fixture.gateway.requests().is_empty());
}

#[tokio::test]
async fn hfp01_actual_factory_executor_missing_requirements_closed_detail() {
    let fixture = HostFixture::new("thread-hfp-missing", "public host-only control").await;
    // Legal control must pass before the missing-metadata assertion.
    fixture
        .factory()
        .create_host(&fixture.claimed)
        .await
        .expect("legal original control");
    let claimed = planned_claim(&fixture).await;
    let (executor, store) = executor(&fixture, &claimed, Arc::new(fixture.factory()));
    let error = executor
        .execute_claimed_run(claimed, store)
        .await
        .expect_err("actual factory must reject missing requirements");
    assert_eq!(error.failure_category(), "host_creation_failed");
    assert_eq!(
        error.failure().detail(),
        Some("host-safe.host-create.driver-requirements.invalid")
    );
    assert!(fixture.gateway.requests().is_empty());
}

struct TypedRejectingPortFactory;
#[async_trait]
impl LoopCapabilityPortFactory for TypedRejectingPortFactory {
    async fn create_capability_port(
        &self,
        _context: &LoopRunContext,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::ScopeMismatch,
            PRIVATE_REASON,
        ))
    }
}
#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);
impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("captured log lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn hfp02_actual_factory_target_never_logs_private_reason() {
    let fixture = HostFixture::new("thread-hfp-private", "public host-only control").await;
    fixture
        .factory()
        .create_host(&fixture.claimed)
        .await
        .expect("legal original control");
    let claimed = planned_claim(&fixture).await;
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(LogWriter(bytes.clone()))
        .with_max_level(tracing::Level::TRACE)
        .with_target(true)
        .without_time()
        .with_ansi(false)
        .finish();
    let error = {
        let _guard = tracing::subscriber::set_default(subscriber);
        let factory = fixture
            .factory()
            .with_driver_requirements(driver_requirements_for(
                &claimed.resolved_run_profile.loop_driver,
                DriverRequirements::all_required(),
            ))
            .with_profiled_capability_port_factory(
                Arc::new(TypedRejectingPortFactory),
                Arc::new(StaticCapabilitySurfaceProfileResolver::new(
                    CapabilityAllowSet::allowlist([]),
                )),
            );
        match factory.create_host(&claimed).await {
            Ok(_) => panic!("actual typed port rejection must fail"),
            Err(error) => error,
        }
    };
    // The original private reason remains available in RAM; it is not public diagnostic data.
    assert!(error.reason.contains(PRIVATE_REASON));
    let captured =
        String::from_utf8(bytes.lock().expect("captured logs").clone()).expect("UTF8 logs");
    assert!(
        captured.contains("ironclaw_runner::loop_driver_host"),
        "real logging target was not enabled"
    );
    assert!(
        captured.contains("create_capability_port"),
        "actual factory stage was not observed"
    );
    assert!(
        !captured.contains(PRIVATE_REASON),
        "actual target leaked private factory reason"
    );
    assert!(fixture.gateway.requests().is_empty());
}

struct LegacyLiteralFactory {
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl HostFactory for LegacyLiteralFactory {
    async fn create_host(
        &self,
        _claimed: &ClaimedTurnRun,
    ) -> Result<Box<dyn AgentLoopDriverHost + Send + Sync>, HostFactoryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(HostFactoryError {
            reason: format!(
                "{PRIVATE_REASON}:diag:host-safe.host-create.context-scope.scope-mismatch"
            ),
        })
    }
}

#[tokio::test]
async fn hfp03_legacy_struct_literal_and_single_dispatch_do_not_mint_diagnostic() {
    let literal = HostFactoryError {
        reason: "legacy exact reason".into(),
    };
    assert_eq!(
        literal.to_string(),
        "host factory error: legacy exact reason"
    );
    assert_eq!(HostFactoryError::new("legacy exact reason"), literal);
    let fixture = HostFixture::new("thread-hfp-legacy", "public host-only control").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (executor, store) = executor(
        &fixture,
        &fixture.claimed,
        Arc::new(LegacyLiteralFactory {
            calls: calls.clone(),
        }),
    );
    let error = executor
        .execute_claimed_run(fixture.claimed.clone(), store)
        .await
        .expect_err("legacy failure");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(error.failure_category(), "host_creation_failed");
    assert_eq!(error.failure().detail(), None);
    assert!(
        !serde_json::to_string(error.failure())
            .expect("failure JSON")
            .contains(PRIVATE_REASON)
    );
    assert!(fixture.gateway.requests().is_empty());
}

struct InterleavingPortFactory {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl LoopCapabilityPortFactory for InterleavingPortFactory {
    async fn create_capability_port(
        &self,
        _context: &LoopRunContext,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        self.entered.notify_one();
        self.release.notified().await;
        Err(AgentLoopHostError::new(
            AgentLoopHostErrorKind::ScopeMismatch,
            PRIVATE_REASON,
        ))
    }
}

#[tokio::test]
async fn hfp03_concurrent_actual_failures_keep_distinct_stage() {
    let first = HostFixture::new("thread-hfp-concurrent-a", "public host-only control").await;
    let second = HostFixture::new("thread-hfp-concurrent-b", "public host-only control").await;
    first
        .factory()
        .create_host(&first.claimed)
        .await
        .expect("first legal control");
    second
        .factory()
        .create_host(&second.claimed)
        .await
        .expect("second legal control");
    let planned = planned_claim(&first).await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut requirements = driver_requirements_for(
        &planned.resolved_run_profile.loop_driver,
        DriverRequirements::all_required(),
    );
    requirements.extend(driver_requirements_for(
        &second.claimed.resolved_run_profile.loop_driver,
        DriverRequirements::all_optional(),
    ));
    // Both executor calls hold THIS SAME factory, not one factory per run.
    let factory = Arc::new(
        first
            .factory()
            .with_driver_requirements(requirements)
            .with_profiled_capability_port_factory(
                Arc::new(InterleavingPortFactory {
                    entered: entered.clone(),
                    release: release.clone(),
                }),
                Arc::new(StaticCapabilitySurfaceProfileResolver::new(
                    CapabilityAllowSet::allowlist([]),
                )),
            ),
    );
    let (a, a_store) = executor(&first, &planned, factory.clone());
    let mut wrong_scope = second.claimed.clone();
    wrong_scope.state.scope.tenant_id = TenantId::new("hfp-other-tenant").expect("negative tenant");
    let (b, b_store) = executor(&second, &wrong_scope, factory.clone());
    let original_a = tokio::spawn(async move { a.execute_claimed_run(planned, a_store).await });
    let entered_result =
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified()).await;
    let b_result = b.execute_claimed_run(wrong_scope, b_store).await;
    // Release and join before every behavior assertion, including on fixture failure.
    release.notify_one();
    let a_result = original_a.await.expect("first original task joined");
    assert!(
        entered_result.is_ok(),
        "actual async capability-port stage was not entered"
    );
    let a_error = a_result.expect_err("first original failure");
    let b_error = b_result.expect_err("second original failure");
    assert_eq!(a_error.failure_category(), "host_creation_failed");
    assert_eq!(b_error.failure_category(), "host_creation_failed");
    assert_eq!(
        a_error.failure().detail(),
        Some("host-safe.host-create.capability-port.scope-mismatch")
    );
    assert_eq!(
        b_error.failure().detail(),
        Some("host-safe.host-create.context-scope.scope-mismatch")
    );
    assert!(first.gateway.requests().is_empty() && second.gateway.requests().is_empty());
}

#[tokio::test]
async fn hfp03_default_diagnostic_method_is_object_safe_single_call_and_redacted() {
    let fixture = HostFixture::new("thread-hfp-default-api", "public host-only control").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let factory: Arc<dyn HostFactory> = Arc::new(LegacyLiteralFactory {
        calls: calls.clone(),
    });
    let failure = match factory.create_host_with_diagnostics(&fixture.claimed).await {
        Err(failure) => failure,
        Ok(_) => panic!("original legacy factory rejects"),
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(failure.diagnostic(), None);
    assert!(!format!("{failure:?}").contains(PRIVATE_REASON));
    assert!(!failure.to_string().contains(PRIVATE_REASON));
    assert!(failure.into_legacy_error().reason.contains(PRIVATE_REASON));
    let _ = factory.create_host(&fixture.claimed).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(fixture.gateway.requests().is_empty());
}

struct CountingRejectingPortFactory(Arc<AtomicUsize>);
#[async_trait]
impl LoopCapabilityPortFactory for CountingRejectingPortFactory {
    async fn create_capability_port(
        &self,
        context: &LoopRunContext,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        TypedRejectingPortFactory
            .create_capability_port(context)
            .await
    }
}

#[tokio::test]
async fn hfp03_actual_old_and_diagnostic_entry_each_build_once() {
    let fixture = HostFixture::new("thread-hfp-single-build", "public host-only control").await;
    let claimed = planned_claim(&fixture).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let factory: Arc<dyn HostFactory> = Arc::new(
        fixture
            .factory()
            .with_driver_requirements(driver_requirements_for(
                &claimed.resolved_run_profile.loop_driver,
                DriverRequirements::all_required(),
            ))
            .with_profiled_capability_port_factory(
                Arc::new(CountingRejectingPortFactory(calls.clone())),
                Arc::new(StaticCapabilitySurfaceProfileResolver::new(
                    CapabilityAllowSet::allowlist([]),
                )),
            ),
    );
    let legacy = match factory.create_host(&claimed).await {
        Err(error) => error,
        Ok(_) => panic!("original typed port rejects"),
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(legacy.reason.contains(PRIVATE_REASON));
    let failure = match factory.create_host_with_diagnostics(&claimed).await {
        Err(error) => error,
        Ok(_) => panic!("original typed port rejects"),
    };
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        failure
            .diagnostic()
            .expect("typed per-build diagnostic")
            .detail_code(),
        "host-safe.host-create.capability-port.scope-mismatch"
    );
    assert!(!format!("{failure:?}").contains(PRIVATE_REASON));
    assert!(!failure.to_string().contains(PRIVATE_REASON));
    assert!(failure.into_legacy_error().reason.contains(PRIVATE_REASON));
    assert!(fixture.gateway.requests().is_empty());
}
