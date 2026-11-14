use async_trait::async_trait;
use ironclaw_observability::live_latency_started_at;
use std::{
    collections::HashMap,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
    time::Instant,
};
use tracing::debug;

const MAX_PREPARED_RUN_IDS: usize = 4096;

fn trace_coordinator_latency_ok(
    operation: &'static str,
    scope: &TurnScope,
    run_id: Option<TurnRunId>,
    started_at: Option<Instant>,
) {
    let run_id = run_id.map(|id| id.to_string()).unwrap_or_default();
    ironclaw_observability::live_latency_trace_ok!(
        "turn_coordinator",
        operation,
        started_at,
        tenant_id = %scope.tenant_id,
        agent_id = scope.agent_id.as_ref().map(|id| id.as_str()).unwrap_or(""),
        project_id = scope.project_id.as_ref().map(|id| id.as_str()).unwrap_or(""),
        thread_id = %scope.thread_id,
        owner_user_id = scope.explicit_owner_user_id().map(|id| id.as_str()).unwrap_or(""),
        run_id = run_id.as_str(),
        "turn coordinator operation completed",
    );
}

fn trace_coordinator_latency_error<E: ?Sized>(
    operation: &'static str,
    scope: &TurnScope,
    run_id: Option<TurnRunId>,
    started_at: Option<Instant>,
    _error: &E,
) {
    let run_id = run_id.map(|id| id.to_string()).unwrap_or_default();
    ironclaw_observability::live_latency_trace_error!(
        "turn_coordinator",
        operation,
        started_at,
        "turn_error",
        tenant_id = %scope.tenant_id,
        agent_id = scope.agent_id.as_ref().map(|id| id.as_str()).unwrap_or(""),
        project_id = scope.project_id.as_ref().map(|id| id.as_str()).unwrap_or(""),
        thread_id = %scope.thread_id,
        owner_user_id = scope.explicit_owner_user_id().map(|id| id.as_str()).unwrap_or(""),
        run_id = run_id.as_str(),
        "turn coordinator operation failed",
    );
}

use crate::{
    AdmissionRejection, CancelRunRequest, CancelRunResponse, GetRunStateRequest,
    InMemoryRunProfileResolver, RecoverTurnRequest, ResumeTurnRequest, ResumeTurnResponse,
    RetryTurnRequest, RetryTurnResponse, RunProfileId, RunProfileVersion, RunProfileResolver,
    SubmitChildRunRequest, SubmitTurnRequest,
    SubmitTurnResponse, TurnCapacityResource, TurnError, TurnRunId, TurnRunState, TurnScope,
    TurnSpawnTreeStateStore, TurnStateStore, TurnStatus,
    events::EventCursor,
    lifecycle::{LifecyclePublicationErrorPort, NoopLifecyclePublicationErrorPort},
};

mod product_profile;
use product_profile::ProductProfileResolver;

pub trait TurnAdmissionPolicy: Send + Sync {
    fn check_submit(&self, request: &SubmitTurnRequest) -> Result<(), AdmissionRejection>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRunWake {
    pub scope: TurnScope,
    pub run_id: TurnRunId,
    pub status: TurnStatus,
    pub event_cursor: EventCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TurnRunWakeNotifyError {
    DeliveryUnavailable,
}

impl fmt::Display for TurnRunWakeNotifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeliveryUnavailable => formatter.write_str("delivery_unavailable"),
        }
    }
}

impl std::error::Error for TurnRunWakeNotifyError {}

pub trait TurnRunWakeNotifier: Send + Sync {
    fn notify_queued_run(&self, wake: TurnRunWake) -> Result<(), TurnRunWakeNotifyError>;
}

#[derive(Debug, Default)]
pub struct NoopTurnRunWakeNotifier;

impl TurnRunWakeNotifier for NoopTurnRunWakeNotifier {
    fn notify_queued_run(&self, _wake: TurnRunWake) -> Result<(), TurnRunWakeNotifyError> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct AllowAllTurnAdmissionPolicy;

impl TurnAdmissionPolicy for AllowAllTurnAdmissionPolicy {
    fn check_submit(&self, _request: &SubmitTurnRequest) -> Result<(), AdmissionRejection> {
        Ok(())
    }
}

#[async_trait]
pub trait TurnCoordinator: Send + Sync {
    /// Mint a run id without side effects. This intentionally has no default
    /// implementation so every coordinator opts into prepared-run semantics.
    async fn prepare_turn(&self, scope: TurnScope) -> Result<TurnRunId, TurnError>;

    /// Reserve a caller-generated run ID against its canonical scope before
    /// submit. Product adapters use this only when a trusted upstream must
    /// bind durable task state before the run is queued.
    async fn reserve_prepared_turn_id(
        &self,
        _scope: TurnScope,
        _run_id: TurnRunId,
    ) -> Result<(), TurnError> {
        Err(TurnError::Unauthorized)
    }

    /// Release a run id minted by `prepare_turn` when the caller fails before
    /// submitting it. Coordinators without a prepared-id cache can treat this
    /// as a no-op.
    async fn abort_prepared_turn(&self, _run_id: TurnRunId) -> Result<(), TurnError> {
        Ok(())
    }

    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResponse, TurnError>;

    /// Submit a root after a host product service has authorized its exact driver
    /// profile. Public message adapters must continue to use `submit_turn`.
    async fn submit_product_profile_turn(
        &self,
        _request: SubmitTurnRequest,
        _profile_id: RunProfileId,
        _profile_version: RunProfileVersion,
    ) -> Result<SubmitTurnResponse, TurnError> {
        Err(TurnError::Unauthorized)
    }

    async fn product_profile_ready(&self, _profile: &RunProfileId, _version: RunProfileVersion) -> bool {
        false
    }

    async fn resume_turn(
        &self,
        request: ResumeTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError>;

    async fn retry_turn(&self, request: RetryTurnRequest) -> Result<RetryTurnResponse, TurnError>;

    /// Re-drive one exact failed special-driver root without changing its run id.
    async fn recover_turn(
        &self,
        request: RecoverTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError> {
        Err(TurnError::RunNotRetryable {
            run_id: request.run_id,
        })
    }

    async fn cancel_run(&self, request: CancelRunRequest) -> Result<CancelRunResponse, TurnError>;

    async fn get_run_state(&self, request: GetRunStateRequest) -> Result<TurnRunState, TurnError>;
}

#[async_trait]
pub trait TurnSpawnTreePort: Send + Sync {
    /// Submit a child run by deriving lineage from the persisted parent and
    /// holding the spawn-tree reservation around the underlying turn submit.
    async fn submit_child_run(
        &self,
        request: SubmitChildRunRequest,
    ) -> Result<SubmitTurnResponse, TurnError>;

    /// Derive a child with the same protected profile as its durable parent.
    async fn submit_product_profile_child(
        &self,
        _request: SubmitChildRunRequest,
        _profile: RunProfileId,
        _version: RunProfileVersion,
    ) -> Result<SubmitTurnResponse, TurnError> {
        Err(TurnError::Unauthorized)
    }
}

pub struct DefaultTurnCoordinator<S: ?Sized> {
    store: Arc<S>,
    admission_policy: Arc<dyn TurnAdmissionPolicy>,
    run_profile_resolver: Arc<dyn RunProfileResolver>,
    wake_notifier: Arc<dyn TurnRunWakeNotifier>,
    publication_error_port: Arc<dyn LifecyclePublicationErrorPort>,
    // Per-coordinator binding of run ids handed out by `prepare_turn` to the
    // scope they were prepared under. `submit_turn` consumes the reservation
    // when `requested_run_id` is set and rejects cross-scope submission so a
    // prepared id cannot be used to inject lineage into a different scope.
    prepared_run_id_scopes: Mutex<HashMap<TurnRunId, TurnScope>>,
}

impl<S> DefaultTurnCoordinator<S>
where
    S: TurnStateStore + ?Sized,
{
    pub fn new(store: Arc<S>) -> Self {
        Self {
            store,
            admission_policy: Arc::new(AllowAllTurnAdmissionPolicy),
            run_profile_resolver: Arc::new(InMemoryRunProfileResolver::default()),
            wake_notifier: Arc::new(NoopTurnRunWakeNotifier),
            publication_error_port: Arc::new(NoopLifecyclePublicationErrorPort),
            prepared_run_id_scopes: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_admission_policy(mut self, policy: Arc<dyn TurnAdmissionPolicy>) -> Self {
        self.admission_policy = policy;
        self
    }

    pub fn with_run_profile_resolver(mut self, resolver: Arc<dyn RunProfileResolver>) -> Self {
        self.run_profile_resolver = resolver;
        self
    }

    pub fn with_wake_notifier(mut self, notifier: Arc<dyn TurnRunWakeNotifier>) -> Self {
        self.wake_notifier = notifier;
        self
    }

    pub fn with_lifecycle_publication_error_port(
        mut self,
        port: Arc<dyn LifecyclePublicationErrorPort>,
    ) -> Self {
        self.publication_error_port = port;
        self
    }

    fn record_prepared_run_id(&self, run_id: TurnRunId, scope: TurnScope) -> Result<(), TurnError> {
        let mut prepared = match self.prepared_run_id_scopes.lock() {
            Ok(prepared) => prepared,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(existing_scope) = prepared.get(&run_id) {
            return if existing_scope == &scope {
                Ok(())
            } else {
                Err(TurnError::Unauthorized)
            };
        }
        if prepared.len() >= MAX_PREPARED_RUN_IDS {
            return Err(TurnError::CapacityExceeded {
                resource: TurnCapacityResource::SubmitTurn,
                cap: MAX_PREPARED_RUN_IDS as u64,
            });
        }
        prepared.insert(run_id, scope);
        Ok(())
    }

    fn consume_prepared_run_id(&self, run_id: TurnRunId) -> Option<TurnScope> {
        let mut prepared = match self.prepared_run_id_scopes.lock() {
            Ok(prepared) => prepared,
            Err(poisoned) => poisoned.into_inner(),
        };
        prepared.remove(&run_id)
    }
}

fn submit_wake(scope: TurnScope, response: &SubmitTurnResponse) -> TurnRunWake {
    let SubmitTurnResponse::Accepted {
        run_id,
        status,
        event_cursor,
        ..
    } = response;
    TurnRunWake {
        scope,
        run_id: *run_id,
        status: *status,
        event_cursor: *event_cursor,
    }
}

fn submit_event_cursor(response: &SubmitTurnResponse) -> EventCursor {
    let SubmitTurnResponse::Accepted { event_cursor, .. } = response;
    *event_cursor
}

fn resume_wake(scope: TurnScope, response: &ResumeTurnResponse) -> TurnRunWake {
    TurnRunWake {
        scope,
        run_id: response.run_id,
        status: response.status,
        event_cursor: response.event_cursor,
    }
}

fn retry_wake(scope: TurnScope, response: &RetryTurnResponse) -> TurnRunWake {
    TurnRunWake {
        scope,
        run_id: response.run_id,
        status: response.status,
        event_cursor: response.event_cursor,
    }
}

fn cancel_wake(scope: TurnScope, response: &CancelRunResponse) -> TurnRunWake {
    TurnRunWake {
        scope,
        run_id: response.run_id,
        status: response.status,
        event_cursor: response.event_cursor,
    }
}

fn notify_queued_run_best_effort(notifier: &dyn TurnRunWakeNotifier, wake: TurnRunWake) {
    match catch_unwind(AssertUnwindSafe(|| notifier.notify_queued_run(wake))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => debug!(error = %error, "turn run wake notification failed"),
        Err(_) => debug!("turn run wake notifier panicked"),
    }
}

fn deferred_publication_error(
    port: &dyn LifecyclePublicationErrorPort,
    cursor: EventCursor,
) -> Result<(), TurnError> {
    match port.take_lifecycle_publication_error(cursor) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

impl<S> DefaultTurnCoordinator<S>
where
    S: TurnStateStore + ?Sized,
{
    async fn submit_bound_turn(
        &self,
        request: SubmitTurnRequest,
        product_profile: Option<&ProductProfileResolver<'_>>,
    ) -> Result<SubmitTurnResponse, TurnError> {
        let started_at = live_latency_started_at();
        // If the caller passed a run id that came out of prepare_turn, verify
        // it is being submitted under the same scope it was prepared under,
        // unless this is a child run (parent_run_id set). Subagent spawn
        // legitimately prepares a run id in the parent scope and submits it
        // under a different child scope. Reservations are consumed on the
        // first submit attempt; a second attempt with the same id falls back
        // to the store's duplicate-bound check.
        if let Some(requested) = request.requested_run_id
            && let Some(prepared_scope) = self.consume_prepared_run_id(requested)
            && request.parent_run_id.is_none()
            && prepared_scope != request.scope
        {
            return Err(TurnError::Unauthorized);
        }
        let scope = request.scope.clone();
        let expected_product = product_profile.map(|profile| {
            (
                profile,
                request.requested_run_id,
                request.accepted_message_ref.clone(),
                request.source_binding_ref.clone(),
                request.reply_target_binding_ref.clone(),
            )
        });
        let resolver: &dyn RunProfileResolver = match product_profile {
            Some(profile) => profile,
            None => self.run_profile_resolver.as_ref(),
        };
        let response = match self
            .store
            .submit_turn(
                request,
                self.admission_policy.as_ref(),
                resolver,
            )
            .await
        {
            Ok(response) => {
                let SubmitTurnResponse::Accepted { run_id, .. } = &response;
                trace_coordinator_latency_ok(
                    "store_submit_turn",
                    &scope,
                    Some(*run_id),
                    started_at,
                );
                response
            }
            Err(error) => {
                trace_coordinator_latency_error(
                    "store_submit_turn",
                    &scope,
                    None,
                    started_at,
                    &error,
                );
                return Err(error);
            }
        };
        let SubmitTurnResponse::Accepted {
            run_id,
            status,
            resolved_run_profile_id,
            resolved_run_profile_version,
            ..
        } = &response;
        debug!(
            run_id = %run_id,
            status = ?status,
            resolved_run_profile_id = resolved_run_profile_id.as_str(),
            resolved_run_profile_version = resolved_run_profile_version.as_u64(),
            "turn coordinator accepted turn with resolved run profile"
        );
        if let Some((profile, expected_run, accepted_message, source, reply_target)) = expected_product {
            let state = self.store.get_run_state(GetRunStateRequest {
                scope: scope.clone(),
                run_id: *run_id,
            }).await?;
            if expected_run != Some(*run_id)
                || resolved_run_profile_id != &profile.id
                || resolved_run_profile_version != &profile.version
                || state.scope != scope
                || state.run_id != *run_id
                || state.resolved_run_profile_id != profile.id
                || state.resolved_run_profile_version != profile.version
                || state.accepted_message_ref != accepted_message
                || state.source_binding_ref != source
                || state.reply_target_binding_ref != reply_target
            {
                return Err(TurnError::Conflict {
                    reason: "stored host product submission does not match the requested identity".into(),
                });
            }
        }
        let wake_started_at = live_latency_started_at();
        notify_queued_run_best_effort(
            self.wake_notifier.as_ref(),
            submit_wake(scope.clone(), &response),
        );
        trace_coordinator_latency_ok("notify_queued_run", &scope, Some(*run_id), wake_started_at);
        deferred_publication_error(
            self.publication_error_port.as_ref(),
            submit_event_cursor(&response),
        )?;
        Ok(response)
    }
}

#[async_trait]
impl<S> TurnCoordinator for DefaultTurnCoordinator<S>
where
    S: TurnStateStore + ?Sized + 'static,
{
    async fn prepare_turn(&self, scope: TurnScope) -> Result<TurnRunId, TurnError> {
        let run_id = TurnRunId::new();
        self.record_prepared_run_id(run_id, scope)?;
        Ok(run_id)
    }

    async fn reserve_prepared_turn_id(
        &self,
        scope: TurnScope,
        run_id: TurnRunId,
    ) -> Result<(), TurnError> {
        self.record_prepared_run_id(run_id, scope)
    }

    async fn abort_prepared_turn(&self, run_id: TurnRunId) -> Result<(), TurnError> {
        self.consume_prepared_run_id(run_id);
        Ok(())
    }

    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResponse, TurnError> {
        self.submit_bound_turn(request, None).await
    }

    async fn submit_product_profile_turn(
        &self,
        request: SubmitTurnRequest,
        profile_id: RunProfileId,
        profile_version: RunProfileVersion,
    ) -> Result<SubmitTurnResponse, TurnError> {
        if request.requested_run_id.is_none()
            || request.parent_run_id.is_some()
            || request.subagent_depth != 0
            || request.spawn_tree_root_run_id.is_some()
            || request.requested_run_profile.as_ref().map(|profile| profile.as_str())
                != Some(profile_id.as_str())
        {
            return Err(TurnError::InvalidRequest {
                reason: "host product submission requires a root run and exact selected profile".into(),
            });
        }
        let resolver = ProductProfileResolver {
            inner: self.run_profile_resolver.as_ref(),
            id: profile_id,
            version: profile_version,
        };
        self.submit_bound_turn(request, Some(&resolver)).await
    }

    async fn product_profile_ready(&self, profile: &RunProfileId, version: RunProfileVersion) -> bool {
        use crate::{
            RunProfileRequest,
            run_profile::{PrivilegedRunProfileDimension, RunProfileRequestAuthority, RunProfileResolutionRequest},
        };
        let Ok(requested) = RunProfileRequest::new(profile.as_str()) else {
            return false;
        };
        ProductProfileResolver {
            inner: self.run_profile_resolver.as_ref(),
            id: profile.clone(),
            version,
        }
        .resolve_run_profile(
            RunProfileResolutionRequest::interactive_default()
                .with_requested_run_profile(requested)
                .with_authority(RunProfileRequestAuthority::ProductSurface),
        )
        .await
        .is_ok_and(|resolved| {
            resolved.provenance.effective_privileges.contains(&PrivilegedRunProfileDimension::SpecialDriver)
        })
    }

    async fn resume_turn(
        &self,
        request: ResumeTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError> {
        let started_at = live_latency_started_at();
        let scope = request.scope.clone();
        let response = match self.store.resume_turn(request).await {
            Ok(response) => {
                trace_coordinator_latency_ok(
                    "store_resume_turn",
                    &scope,
                    Some(response.run_id),
                    started_at,
                );
                response
            }
            Err(error) => {
                trace_coordinator_latency_error(
                    "store_resume_turn",
                    &scope,
                    None,
                    started_at,
                    &error,
                );
                return Err(error);
            }
        };
        let wake_started_at = live_latency_started_at();
        notify_queued_run_best_effort(
            self.wake_notifier.as_ref(),
            resume_wake(scope.clone(), &response),
        );
        trace_coordinator_latency_ok(
            "notify_resumed_run",
            &scope,
            Some(response.run_id),
            wake_started_at,
        );
        deferred_publication_error(self.publication_error_port.as_ref(), response.event_cursor)?;
        Ok(response)
    }

    async fn retry_turn(&self, request: RetryTurnRequest) -> Result<RetryTurnResponse, TurnError> {
        let scope = request.scope.clone();
        let response = self.store.retry_turn(request).await?;
        notify_queued_run_best_effort(
            self.wake_notifier.as_ref(),
            retry_wake(scope.clone(), &response),
        );
        deferred_publication_error(self.publication_error_port.as_ref(), response.event_cursor)?;
        Ok(response)
    }

    async fn recover_turn(
        &self,
        request: RecoverTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError> {
        let scope = request.scope.clone();
        let response = self.store.recover_turn(request).await?;
        notify_queued_run_best_effort(
            self.wake_notifier.as_ref(),
            resume_wake(scope.clone(), &response),
        );
        deferred_publication_error(self.publication_error_port.as_ref(), response.event_cursor)?;
        Ok(response)
    }

    async fn cancel_run(&self, request: CancelRunRequest) -> Result<CancelRunResponse, TurnError> {
        let started_at = live_latency_started_at();
        let scope = request.scope.clone();
        let response = match self.store.request_cancel(request).await {
            Ok(response) => {
                trace_coordinator_latency_ok(
                    "store_cancel_run",
                    &scope,
                    Some(response.run_id),
                    started_at,
                );
                response
            }
            Err(error) => {
                trace_coordinator_latency_error(
                    "store_cancel_run",
                    &scope,
                    None,
                    started_at,
                    &error,
                );
                return Err(error);
            }
        };
        // Wake on `CancelRequested` (the cooperative case) AND on any terminal
        // transition. Registered handles otherwise rely solely on the polling
        // fallback to discover a direct-to-terminal cancellation, which would
        // leave them in the requester map until the polling task next ticks.
        if response.status == TurnStatus::CancelRequested || response.status.is_terminal() {
            let wake_started_at = live_latency_started_at();
            notify_queued_run_best_effort(
                self.wake_notifier.as_ref(),
                cancel_wake(scope.clone(), &response),
            );
            trace_coordinator_latency_ok(
                "notify_cancelled_run",
                &scope,
                Some(response.run_id),
                wake_started_at,
            );
        }
        if !response.already_terminal {
            deferred_publication_error(
                self.publication_error_port.as_ref(),
                response.event_cursor,
            )?;
        }
        Ok(response)
    }

    async fn get_run_state(&self, request: GetRunStateRequest) -> Result<TurnRunState, TurnError> {
        self.store.get_run_state(request).await
    }
}

impl<S> DefaultTurnCoordinator<S>
where
    S: TurnSpawnTreeStateStore + ?Sized,
{
    async fn submit_child_with_resolver(
        &self,
        request: SubmitChildRunRequest,
        product_profile: Option<&ProductProfileResolver<'_>>,
    ) -> Result<SubmitTurnResponse, TurnError> {
        let started_at = live_latency_started_at();
        let child_scope = request.child_scope.clone();
        let expected = product_profile.map(|profile| {
            (
                profile,
                request.requested_run_id,
                request.actor.clone(),
                request.accepted_message_ref.clone(),
                request.source_binding_ref.clone(),
                request.reply_target_binding_ref.clone(),
            )
        });
        let resolver: &dyn RunProfileResolver = match product_profile {
            Some(profile) => profile,
            None => self.run_profile_resolver.as_ref(),
        };
        let response = match self
            .store
            .submit_child_turn(
                request,
                self.admission_policy.as_ref(),
                resolver,
            )
            .await
        {
            Ok(response) => {
                let SubmitTurnResponse::Accepted { run_id, .. } = &response;
                trace_coordinator_latency_ok(
                    "store_submit_child_turn",
                    &child_scope,
                    Some(*run_id),
                    started_at,
                );
                response
            }
            Err(error) => {
                trace_coordinator_latency_error(
                    "store_submit_child_turn",
                    &child_scope,
                    None,
                    started_at,
                    &error,
                );
                return Err(error);
            }
        };
        let SubmitTurnResponse::Accepted { run_id, .. } = &response;
        if let Some((profile, expected_run, actor, accepted_message, source, reply_target)) = expected {
            let state = self.store.get_run_state(GetRunStateRequest {
                scope: child_scope.clone(),
                run_id: *run_id,
            }).await?;
            if expected_run != Some(*run_id)
                || state.scope != child_scope
                || state.run_id != *run_id
                || state.actor.as_ref() != Some(&actor)
                || state.resolved_run_profile_id != profile.id
                || state.resolved_run_profile_version != profile.version
                || state.accepted_message_ref != accepted_message
                || state.source_binding_ref != source
                || state.reply_target_binding_ref != reply_target
            {
                return Err(TurnError::Conflict {
                    reason: "stored protected child submission does not match the requested identity".into(),
                });
            }
        }
        let wake_started_at = live_latency_started_at();
        notify_queued_run_best_effort(
            self.wake_notifier.as_ref(),
            submit_wake(child_scope.clone(), &response),
        );
        trace_coordinator_latency_ok(
            "notify_child_run",
            &child_scope,
            Some(*run_id),
            wake_started_at,
        );
        deferred_publication_error(
            self.publication_error_port.as_ref(),
            submit_event_cursor(&response),
        )?;
        Ok(response)
    }
}

#[async_trait]
impl<S> TurnSpawnTreePort for DefaultTurnCoordinator<S>
where
    S: TurnSpawnTreeStateStore + ?Sized + 'static,
{
    async fn submit_child_run(
        &self,
        request: SubmitChildRunRequest,
    ) -> Result<SubmitTurnResponse, TurnError> {
        self.submit_child_with_resolver(request, None).await
    }

    async fn submit_product_profile_child(
        &self,
        mut request: SubmitChildRunRequest,
        profile: RunProfileId,
        version: RunProfileVersion,
    ) -> Result<SubmitTurnResponse, TurnError> {
        if request.requested_run_id.is_none()
            || request.requested_run_profile.as_ref().map(|hint| hint.as_str()) != Some(profile.as_str())
            || request.parent_scope.explicit_owner_user_id() != Some(&request.actor.user_id)
            || request.child_scope.explicit_owner_user_id() != Some(&request.actor.user_id)
            || !self.product_profile_ready(&profile, version).await
        {
            return Err(TurnError::Unauthorized);
        }
        let parent = self.store.get_run_state(GetRunStateRequest {
            scope: request.parent_scope.clone(),
            run_id: request.parent_run_id,
        }).await?;
        if parent.status != TurnStatus::Running
            || parent.scope != request.parent_scope
            || parent.run_id != request.parent_run_id
            || parent.actor.as_ref() != Some(&request.actor)
            || parent.resolved_run_profile_id != profile
            || parent.resolved_run_profile_version != version
        {
            return Err(TurnError::Unauthorized);
        }
        request.spawn_tree_descendant_cap = request.spawn_tree_descendant_cap.min(1);
        let resolver = ProductProfileResolver {
            inner: self.run_profile_resolver.as_ref(),
            id: profile,
            version,
        };
        self.submit_child_with_resolver(request, Some(&resolver)).await
    }
}

#[async_trait]
impl<C> TurnCoordinator for Arc<C>
where
    C: TurnCoordinator + ?Sized,
{
    async fn reserve_prepared_turn_id(
        &self,
        scope: TurnScope,
        run_id: TurnRunId,
    ) -> Result<(), TurnError> {
        self.as_ref().reserve_prepared_turn_id(scope, run_id).await
    }

    async fn prepare_turn(&self, scope: TurnScope) -> Result<TurnRunId, TurnError> {
        self.as_ref().prepare_turn(scope).await
    }

    async fn abort_prepared_turn(&self, run_id: TurnRunId) -> Result<(), TurnError> {
        self.as_ref().abort_prepared_turn(run_id).await
    }

    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResponse, TurnError> {
        self.as_ref().submit_turn(request).await
    }

    async fn submit_product_profile_turn(
        &self,
        request: SubmitTurnRequest,
        profile_id: RunProfileId,
        profile_version: RunProfileVersion,
    ) -> Result<SubmitTurnResponse, TurnError> {
        self.as_ref()
            .submit_product_profile_turn(request, profile_id, profile_version)
            .await
    }

    async fn product_profile_ready(&self, profile: &RunProfileId, version: RunProfileVersion) -> bool {
        self.as_ref().product_profile_ready(profile, version).await
    }

    async fn resume_turn(
        &self,
        request: ResumeTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError> {
        self.as_ref().resume_turn(request).await
    }

    async fn retry_turn(&self, request: RetryTurnRequest) -> Result<RetryTurnResponse, TurnError> {
        self.as_ref().retry_turn(request).await
    }

    async fn recover_turn(
        &self,
        request: RecoverTurnRequest,
    ) -> Result<ResumeTurnResponse, TurnError> {
        self.as_ref().recover_turn(request).await
    }

    async fn cancel_run(&self, request: CancelRunRequest) -> Result<CancelRunResponse, TurnError> {
        self.as_ref().cancel_run(request).await
    }

    async fn get_run_state(&self, request: GetRunStateRequest) -> Result<TurnRunState, TurnError> {
        self.as_ref().get_run_state(request).await
    }
}

#[async_trait]
impl<C> TurnSpawnTreePort for Arc<C>
where
    C: TurnSpawnTreePort + ?Sized,
{
    async fn submit_child_run(
        &self,
        request: SubmitChildRunRequest,
    ) -> Result<SubmitTurnResponse, TurnError> {
        self.as_ref().submit_child_run(request).await
    }
    async fn submit_product_profile_child(
        &self,
        request: SubmitChildRunRequest,
        profile: RunProfileId,
        version: RunProfileVersion,
    ) -> Result<SubmitTurnResponse, TurnError> {
        self.as_ref().submit_product_profile_child(request, profile, version).await
    }

}

