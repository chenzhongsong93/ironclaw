//! 正常Production端口上的result_read availability与实际dispatch；无模型/HTTP/PG外部服务。
use super::*;
use ironclaw_host_api::{
    CapabilityId, ProviderToolName, Resolution, ResolutionBatch, ThreadId, UserId,
};
use ironclaw_loop_host::{CapabilitySurfaceProfileFilter, CapabilitySurfaceProfileResolver};
use ironclaw_threads::{
    AppendToolResultReferenceRequest, EnsureThreadRequest, PutToolResultRecordRequest,
    ReadToolResultRecordRequest, ThreadScope, ToolResultSafeSummary,
};
use ironclaw_turns::run_profile::{
    AgentLoopHostError, CapabilityBatchInvocation, CapabilityCallCandidate, CapabilityInvocation,
    LoopCapabilityPort, LoopRunContext, ProviderToolCall, ProviderToolCallCapabilityIds,
    ProviderToolDefinition, RegisterProviderToolCallRequest, VisibleCapabilityRequest,
    VisibleCapabilitySurface,
};

const ID: &str = "builtin.result_read";
const NAME: &str = "builtin__result_read";

// Only narrows a real normal decorated surface; never fabricates a positive handler/descriptor.
struct NarrowAvailability {
    inner: Arc<dyn LoopCapabilityPort>,
    omit_descriptor: bool,
}
#[async_trait]
impl LoopCapabilityPort for NarrowAvailability {
    fn tool_definitions(&self) -> Result<Vec<ProviderToolDefinition>, AgentLoopHostError> {
        self.inner.tool_definitions()
    }
    fn provider_tool_call_capability_ids(
        &self,
        call: &ProviderToolCall,
    ) -> Result<ProviderToolCallCapabilityIds, AgentLoopHostError> {
        self.inner.provider_tool_call_capability_ids(call)
    }
    fn validate_provider_tool_call(
        &self,
        call: &ProviderToolCall,
    ) -> Result<(), AgentLoopHostError> {
        self.inner.validate_provider_tool_call(call)
    }
    async fn register_provider_tool_call(
        &self,
        request: RegisterProviderToolCallRequest,
    ) -> Result<CapabilityCallCandidate, AgentLoopHostError> {
        self.inner.register_provider_tool_call(request).await
    }
    async fn visible_capabilities(
        &self,
        request: VisibleCapabilityRequest,
    ) -> Result<VisibleCapabilitySurface, AgentLoopHostError> {
        let mut surface = self.inner.visible_capabilities(request).await?;
        let mut callable = surface.callable_capability_ids.clone().unwrap_or_else(|| {
            surface
                .descriptors
                .iter()
                .map(|d| d.capability_id.clone())
                .collect()
        });
        if self.omit_descriptor {
            surface
                .descriptors
                .retain(|d| d.capability_id.as_str() != ID);
        } else {
            callable.retain(|id| id.as_str() != ID);
        }
        surface.callable_capability_ids = Some(callable);
        Ok(surface)
    }
    async fn invoke_capability(
        &self,
        request: CapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        self.inner.invoke_capability(request).await
    }
    async fn invoke_capability_batch(
        &self,
        request: CapabilityBatchInvocation,
    ) -> Result<ResolutionBatch, AgentLoopHostError> {
        self.inner.invoke_capability_batch(request).await
    }
}

pub(super) async fn assert_normal_result_read(
    context: &RebornProductionSurfaceContext,
    run: &LoopRunContext,
    port: Arc<dyn LoopCapabilityPort>,
    resolver: Arc<dyn CapabilitySurfaceProfileResolver>,
) {
    let surface = port
        .visible_capabilities(VisibleCapabilityRequest {})
        .await
        .unwrap();
    let read_id = CapabilityId::new(ID).unwrap();
    assert!(
        surface
            .descriptors
            .iter()
            .any(|d| d.capability_id == read_id),
        "positive source is a real installed handler, not a fixture descriptor"
    );
    assert!(
        surface
            .callable_capability_ids
            .as_ref()
            .is_none_or(|ids| ids.contains(&read_id)),
        "real normal decorated availability must admit the installed result reader"
    );
    let allow = resolver
        .resolve_for_capabilities(run, port.as_ref())
        .await
        .unwrap();
    assert!(allow.permits(&read_id));
    let filtered: Arc<dyn LoopCapabilityPort> = Arc::new(CapabilitySurfaceProfileFilter::new(
        port.clone(),
        Arc::new(allow),
    ));
    assert_eq!(
        filtered
            .tool_definitions()
            .unwrap()
            .iter()
            .filter(|d| d.capability_id == read_id && d.name.as_str() == NAME)
            .count(),
        1
    );
    assert_dispatch(context, run, filtered).await;
    for omit_descriptor in [false, true] {
        let narrowed = NarrowAvailability {
            inner: port.clone(),
            omit_descriptor,
        };
        let refused = resolver
            .resolve_for_capabilities(run, &narrowed)
            .await
            .unwrap();
        assert!(
            !refused.permits(&read_id),
            "Some exclusion and descriptor omission both remain closed"
        );
        let filter = CapabilitySurfaceProfileFilter::new(Arc::new(narrowed), Arc::new(refused));
        assert!(
            !filter
                .tool_definitions()
                .unwrap()
                .iter()
                .any(|d| d.capability_id == read_id)
        );
        assert!(
            filter
                .register_provider_tool_call(RegisterProviderToolCallRequest::new(call(
                    run,
                    "result:unavailable-in-narrowed-surface"
                )))
                .await
                .is_err()
        );
    }
}

async fn seed_result(
    context: &RebornProductionSurfaceContext,
    run: &LoopRunContext,
    scope: ThreadScope,
    thread: ThreadId,
    reference: &str,
    content: &[u8],
) {
    context
        .thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: scope.clone(),
            thread_id: Some(thread.clone()),
            created_by_actor_id: "native-surface-owner".into(),
            title: None,
            metadata_json: None,
        })
        .await
        .unwrap();
    context
        .thread_service
        .put_tool_result_record(PutToolResultRecordRequest {
            scope: scope.clone(),
            thread_id: thread.clone(),
            result_ref: reference.into(),
            content: content.to_vec(),
        })
        .await
        .unwrap();
    context
        .thread_service
        .append_tool_result_reference(AppendToolResultReferenceRequest {
            scope,
            thread_id: thread,
            turn_run_id: run.run_id.to_string(),
            result_ref: reference.into(),
            safe_summary: ToolResultSafeSummary::new("mechanical ordinary tool result").unwrap(),
            provider_call: None,
            model_observation: None,
        })
        .await
        .unwrap();
}

fn call(run: &LoopRunContext, reference: &str) -> ProviderToolCall {
    ProviderToolCall {
        provider_id: "native-surface-fixture".into(),
        provider_model_id: "normal-native-surface-fixture".into(),
        turn_id: Some(run.turn_id.to_string()),
        id: format!("native-read-{}", uuid::Uuid::new_v4()),
        name: ProviderToolName::new(NAME).unwrap(),
        arguments: serde_json::json!({"result_ref":reference,"offset":0,"max_bytes":8}),
        response_reasoning: None,
        reasoning: None,
        signature: None,
    }
}

async fn invoke(
    port: &dyn LoopCapabilityPort,
    run: &LoopRunContext,
    reference: &str,
) -> Resolution {
    let candidate = port
        .register_provider_tool_call(RegisterProviderToolCallRequest::new(call(run, reference)))
        .await
        .expect("actual profile-scoped registration must dispatch the installed result reader");
    port.invoke_capability(CapabilityInvocation {
        activity_id: candidate.activity_id,
        surface_version: candidate.surface_version,
        capability_id: candidate.capability_id,
        input_ref: candidate.input_ref,
        approval_resume: None,
        auth_resume: None,
    })
    .await
    .unwrap()
}

async fn assert_dispatch(
    context: &RebornProductionSurfaceContext,
    run: &LoopRunContext,
    port: Arc<dyn LoopCapabilityPort>,
) {
    let base = ThreadScope {
        tenant_id: run.scope.tenant_id.clone(),
        agent_id: run.scope.agent_id.clone().unwrap(),
        project_id: run.scope.project_id.clone(),
        owner_user_id: run.scope.explicit_owner_user_id().cloned(),
        mission_id: None,
    };
    let scope = ironclaw_runner::thread_scope::ThreadScopeResolver::resolve_for_turn(
        &base,
        &run.scope,
        run.actor(),
    );
    let reference = "result:normal-native-read-control";
    let content = b"normal-result-reader-control";
    seed_result(
        context,
        run,
        scope.clone(),
        run.thread_id.clone(),
        reference,
        content,
    )
    .await;
    let outcome = invoke(port.as_ref(), run, reference).await;
    let Resolution::Done(done) = outcome else {
        panic!("actual result_read must complete");
    };
    assert!(done.verdict.error_kind().is_none());
    assert_eq!(done.summary.as_str(), "result chunk returned");
    // These fields are the actual handler observation carried through the
    // original resolution::completed mapping, not independent storage data.
    assert_eq!(
        done.refs.preview.as_ref().map(|preview| preview.as_str()),
        Some(std::str::from_utf8(&content[..8]).unwrap())
    );
    assert_eq!(
        done.refs.preview_meta.total_bytes,
        Some(u64::try_from(content.len()).unwrap())
    );
    assert_eq!(done.refs.preview_meta.next_offset, Some(8));
    assert_eq!(
        done.refs
            .preview_meta
            .referenced_result_ref
            .as_ref()
            .map(|value| value.as_str()),
        Some(reference)
    );
    let reread = context
        .thread_service
        .read_tool_result_record(ReadToolResultRecordRequest {
            scope: scope.clone(),
            thread_id: run.thread_id.clone(),
            result_ref: reference.into(),
            offset: 0,
            max_bytes: 24 * 1024,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reread.content.as_slice(),
        content,
        "original durable result stays byte exact"
    );
    let mut foreign_scope = scope.clone();
    foreign_scope.owner_user_id = Some(UserId::new("other-native-result-owner").unwrap());
    let foreign_thread = ThreadId::new("other-native-result-thread").unwrap();
    seed_result(
        context,
        run,
        foreign_scope,
        foreign_thread,
        "result:foreign-native-read",
        b"foreign-result",
    )
    .await;
    let denied = invoke(port.as_ref(), run, "result:foreign-native-read").await;
    let Resolution::Done(denied) = denied else {
        panic!("foreign result remains recoverable unavailable");
    };
    assert!(denied.verdict.error_kind().is_some());
    assert_eq!(
        denied.summary.as_str(),
        "result reference is unavailable in this thread"
    );
    // A current raw record without a finalized thread reference cannot become readable.
    context
        .thread_service
        .put_tool_result_record(PutToolResultRecordRequest {
            scope,
            thread_id: run.thread_id.clone(),
            result_ref: "result:unreferenced-native-read".into(),
            content: b"unreferenced-result".to_vec(),
        })
        .await
        .unwrap();
    let denied = invoke(port.as_ref(), run, "result:unreferenced-native-read").await;
    let Resolution::Done(denied) = denied else {
        panic!("unreferenced result must remain unavailable");
    };
    assert!(denied.verdict.error_kind().is_some());
    assert_eq!(
        denied.summary.as_str(),
        "result reference is unavailable in this thread"
    );
}
