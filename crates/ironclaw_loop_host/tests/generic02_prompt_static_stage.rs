//! RED-first external integration candidate: no product assets or private PG material.
use std::{collections::BTreeSet, sync::{Arc, Mutex}};
use async_trait::async_trait;
use ironclaw_host_api::{AgentId, TenantId, ThreadId};
use ironclaw_loop_host::{
    SubagentLoopPromptPort, SubagentOpaqueInputPort, SubagentPromptComposer, SubagentPromptGoal,
    SubagentPromptMaterial, SubagentPromptMaterialSource, HostSpawnInputFacts,
    OpaquePromptInputMaterial, OpaquePromptInputReceipt,
};
use ironclaw_turns::{LoopDiagnosticRef, LoopGateRef, TurnId, TurnRunId, TurnScope, run_profile::{
    AgentLoopHostError, AgentLoopHostErrorKind, InMemoryRunProfileResolver,
    LoopInlineMessage, LoopInlineMessageBody, LoopInlineMessageRole,
    LoopModelCapabilityView, AgentLoopHostErrorReasonKind,
    LoopPromptBundle, LoopPromptBundleRequest, LoopPromptPort, LoopRunContext,
    PromptMode, RunProfileResolutionRequest, RunProfileResolver,
}};
use tracing_test::traced_test;

const PRIVATE_CANARY: &str = "PRIVATE_CANARY_error_ids_body_hash";
const PRIVATE_DETAIL: &str = "PRIVATE_DETAIL_CANARY";
const PRIVATE_GATE: &str = "gate:PRIVATE_GATE_CANARY";
const PRIVATE_DIAGNOSTIC: &str = "diag:PRIVATE_DIAGNOSTIC_CANARY";
type Events = Arc<Mutex<Vec<&'static str>>>;
#[derive(Clone, Copy)]
enum FailAt { Source, Opaque, Inner }
fn original_error() -> AgentLoopHostError {
    AgentLoopHostError::new(AgentLoopHostErrorKind::Invalid, PRIVATE_CANARY)
        .with_detail(PRIVATE_DETAIL)
        .with_reason_kind(AgentLoopHostErrorReasonKind::ModelCreditsExhausted)
        .with_gate_ref(LoopGateRef::new(PRIVATE_GATE).expect("supported gate ref"))
        .with_diagnostic_ref(LoopDiagnosticRef::new(PRIVATE_DIAGNOSTIC).expect("supported diagnostic ref"))
}
struct Source { fail: FailAt, events: Events }
#[async_trait]
impl SubagentPromptMaterialSource for Source {
    async fn material_for_run(&self, _: &LoopRunContext) -> Result<SubagentPromptMaterial, AgentLoopHostError> {
        self.events.lock().expect("events").push("source");
        if matches!(self.fail, FailAt::Source) { return Err(original_error()); }
        Ok(SubagentPromptMaterial {
            direction_markdown: "Follow the bounded public task.".into(),
            goal: SubagentPromptGoal { task: "Inspect public fixture.".into(), handoff: None },
            allowed_capabilities: BTreeSet::new(),
        })
    }
}
struct Opaque { fail: FailAt, events: Events }
#[async_trait]
impl SubagentOpaqueInputPort for Opaque {
    async fn bind_before_submit(&self, _: HostSpawnInputFacts<'_>) -> Result<Option<OpaquePromptInputReceipt>, AgentLoopHostError> {
        panic!("prompt-only diagnostic test must not call bind")
    }
    async fn material_for_child(&self, _: &LoopRunContext) -> Result<Option<OpaquePromptInputMaterial>, AgentLoopHostError> {
        self.events.lock().expect("events").push("opaque");
        if matches!(self.fail, FailAt::Opaque) { return Err(original_error()); }
        Ok(None)
    }
}
struct Inner { events: Events, expected: Option<LoopPromptBundleRequest> }
#[async_trait]
impl LoopPromptPort for Inner {
    async fn build_prompt_bundle(&self, request: LoopPromptBundleRequest) -> Result<LoopPromptBundle, AgentLoopHostError> {
        self.events.lock().expect("events").push("inner");
        if let Some(expected) = &self.expected {
            // Entire actual request plus each current public field: no copied expected fixture.
            assert_eq!(&request, expected);
            assert_eq!(request.mode, expected.mode);
            assert_eq!(request.context_cursor, expected.context_cursor);
            assert_eq!(request.surface_version, expected.surface_version);
            assert_eq!(request.capability_view, expected.capability_view);
            assert_eq!(request.checkpoint_state_ref, expected.checkpoint_state_ref);
            assert_eq!(request.max_messages, expected.max_messages);
            assert_eq!(request.inline_messages, expected.inline_messages);
            let bodies: Vec<_> = request.inline_messages.iter().map(|m| m.safe_body.as_str()).collect();
            assert_eq!(bodies.len(), 4);
            assert_eq!(bodies[0], "Follow the bounded public task.");
            assert!(bodies[1].contains("Subagent task."));
            assert_eq!(bodies[2], "Subagent task:\nInspect public fixture.");
            assert_eq!(bodies[3], "PUBLIC_INCOMING_INLINE");
        }
        Err(original_error())
    }
}
async fn actual_caller(fail: FailAt) -> (AgentLoopHostError, Vec<&'static str>) {
    let scope = TurnScope::new(
        TenantId::new("public-tenant").expect("tenant"),
        Some(AgentId::new("public-agent").expect("agent")), None,
        ThreadId::new("public-thread").expect("thread"),
    );
    let profile = InMemoryRunProfileResolver::default()
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default()).await.expect("profile");
    let context = LoopRunContext::new(scope, TurnId::new(), TurnRunId::new(), profile);
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let source = Arc::new(Source { fail, events: events.clone() });
    let opaque = Arc::new(Opaque { fail, events: events.clone() });
    let incoming = LoopPromptBundleRequest {
        mode: PromptMode::TextOnly, context_cursor: None, surface_version: None,
        capability_view: Some(LoopModelCapabilityView { visible_capability_ids: Default::default() }),
        checkpoint_state_ref: None, max_messages: Some(3),
        inline_messages: vec![LoopInlineMessage {
            role: LoopInlineMessageRole::User,
            safe_body: LoopInlineMessageBody::new("PUBLIC_INCOMING_INLINE".to_string()).expect("public inline"),
        }],
    };
    let expected = if matches!(fail, FailAt::Inner) {
        let baseline_events: Events = Arc::new(Mutex::new(Vec::new()));
        let baseline_source = Arc::new(Source { fail: FailAt::Inner, events: baseline_events.clone() });
        let baseline = SubagentPromptComposer::new(baseline_source)
            .compose(&context, incoming.clone()).await.expect("actual composer without opaque");
        assert_eq!(*baseline_events.lock().expect("baseline events"), ["source"]);
        Some(baseline)
    } else { None };
    let inner = Arc::new(Inner { events: events.clone(), expected });
    let composer = SubagentPromptComposer::new(source).with_opaque_input_port(opaque);
    let caller = SubagentLoopPromptPort::new(inner, context, composer);
    let error = match caller.build_prompt_bundle(incoming).await {
        Err(error) => error,
        Ok(_) => panic!("controlled boundary must fail"),
    };
    let recorded = events.lock().expect("events").clone();
    (error, recorded)
}
fn assert_original(error: &AgentLoopHostError) {
    assert_eq!(error.kind, AgentLoopHostErrorKind::Invalid);
    assert_eq!(error.safe_summary, PRIVATE_CANARY);
    let expected = original_error();
    assert_eq!(error.detail, expected.detail);
    assert_eq!(error.reason_kind, expected.reason_kind);
    assert_eq!(error.gate_ref, expected.gate_ref);
    assert_eq!(error.diagnostic_ref, expected.diagnostic_ref);
    assert_eq!(error, &expected, "all six current AgentLoopHostError attributes preserved");
}
#[tokio::test]
#[traced_test]
async fn generic02_prompt_static_source_warn_preserves_error_and_stops_downstream() {
    let (error, events) = actual_caller(FailAt::Source).await;
    assert_original(&error);
    assert_eq!(events, ["source"]);
    assert!(logs_contain("SubagentPromptStageFailed"));
    assert!(logs_contain("stage=\"source\""));
    assert!(logs_contain("WARN"));
    for canary in [PRIVATE_CANARY, PRIVATE_DETAIL, PRIVATE_GATE, PRIVATE_DIAGNOSTIC, "model_credits_exhausted"] {
        assert!(!logs_contain(canary));
    }
    logs_assert(|lines| {
        let events: Vec<_> = lines.iter().filter(|line| line.contains("SubagentPromptStageFailed")).collect();
        if events.len() == 1
            && events[0].contains("WARN")
            && events[0].contains("ironclaw::subagent_prompt")
            && events[0].contains("stage=\"source\"") {
            Ok(())
        } else { Err("exactly one WARN/target/static-stage event on the same line required".into()) }
    });
}
#[tokio::test]
#[traced_test]
async fn generic02_prompt_static_opaque_warn_preserves_error_and_stops_inner() {
    let (error, events) = actual_caller(FailAt::Opaque).await;
    assert_original(&error);
    assert_eq!(events, ["source", "opaque"]);
    assert!(logs_contain("SubagentPromptStageFailed"));
    assert!(logs_contain("stage=\"opaque\""));
    assert!(logs_contain("WARN"));
    for canary in [PRIVATE_CANARY, PRIVATE_DETAIL, PRIVATE_GATE, PRIVATE_DIAGNOSTIC, "model_credits_exhausted"] {
        assert!(!logs_contain(canary));
    }
    logs_assert(|lines| {
        let events: Vec<_> = lines.iter().filter(|line| line.contains("SubagentPromptStageFailed")).collect();
        if events.len() == 1
            && events[0].contains("WARN")
            && events[0].contains("ironclaw::subagent_prompt")
            && events[0].contains("stage=\"opaque\"") {
            Ok(())
        } else { Err("exactly one WARN/target/static-stage event on the same line required".into()) }
    });
}
#[tokio::test]
#[traced_test]
async fn generic02_prompt_static_inner_warn_after_not_required_preserves_error() {
    let (error, events) = actual_caller(FailAt::Inner).await;
    assert_original(&error);
    assert_eq!(events, ["source", "opaque", "inner"]);
    assert!(logs_contain("SubagentPromptStageFailed"));
    assert!(logs_contain("stage=\"inner\""));
    assert!(logs_contain("WARN"));
    for canary in [PRIVATE_CANARY, PRIVATE_DETAIL, PRIVATE_GATE, PRIVATE_DIAGNOSTIC, "model_credits_exhausted"] {
        assert!(!logs_contain(canary));
    }
    logs_assert(|lines| {
        let events: Vec<_> = lines.iter().filter(|line| line.contains("SubagentPromptStageFailed")).collect();
        if events.len() == 1
            && events[0].contains("WARN")
            && events[0].contains("ironclaw::subagent_prompt")
            && events[0].contains("stage=\"inner\"") {
            Ok(())
        } else { Err("exactly one WARN/target/static-stage event on the same line required".into()) }
    });
}

