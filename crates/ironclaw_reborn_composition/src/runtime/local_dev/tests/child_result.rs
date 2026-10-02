use super::*;
use ironclaw_runner::subagent::{
    await_edge::{
        AwaitEdge, AwaitEdgeState, ReservationReleaseState, boot_recovery::run_boot_recovery,
        resolver::AwaitEdgeResolver, store::FilesystemAwaitEdgeStore,
    },
    goal_store::InMemoryBoundedSubagentGoalStore,
};
use ironclaw_turns::{
    DefaultTurnCoordinator, GateRef, SubmitChildRunRequest, SubmitTurnRequest, TurnCoordinator,
    TurnSpawnTreePort, TurnStatus,
    runner::{BlockRunRequest, ClaimRunRequest, CompleteRunRequest, TurnRunTransitionPort},
};

async fn submit_parent(
    coordinator: &dyn TurnCoordinator,
    scope: TurnScope,
    actor: TurnActor,
) -> TurnRunId {
    let ironclaw_turns::SubmitTurnResponse::Accepted { run_id, .. } = coordinator
        .submit_turn(SubmitTurnRequest {
            requested_model: None,
            scope,
            actor,
            accepted_message_ref: AcceptedMessageRef::new("msg:full-parent").unwrap(),
            source_binding_ref: ironclaw_turns::SourceBindingRef::new("source:full-parent")
                .unwrap(),
            reply_target_binding_ref: ReplyTargetBindingRef::new("reply:full-parent").unwrap(),
            requested_run_profile: None,
            idempotency_key: ironclaw_turns::IdempotencyKey::new("idem:full-parent").unwrap(),
            received_at: chrono::Utc::now(),
            requested_run_id: None,
            parent_run_id: None,
            subagent_depth: 0,
            spawn_tree_root_run_id: None,
            product_context: None,
            llm_subject: None,
        })
        .await
        .unwrap();
    run_id
}

fn edge_filesystem(backend: Arc<InMemoryBackend>) -> Arc<ScopedFilesystem<InMemoryBackend>> {
    let grants = MountView::new(vec![MountGrant::new(
        MountAlias::new("/turns").unwrap(),
        VirtualPath::new("/turns").unwrap(),
        MountPermissions::read_write_list_delete(),
    )])
    .unwrap();
    Arc::new(ScopedFilesystem::with_fixed_view(backend, grants))
}

async fn prepare_blocked_child(
    threads: Arc<dyn SessionThreadService>,
    io: Arc<StagedCapabilityIo>,
    backend: Arc<InMemoryBackend>,
    parent: &mut LoopRunContext,
    owner: &UserId,
    text: &str,
) -> (
    Arc<DefaultTurnCoordinator<ironclaw_turns::FilesystemTurnStateRowStore<InMemoryBackend>>>,
    Arc<ironclaw_turns::FilesystemTurnStateRowStore<InMemoryBackend>>,
    TurnScope,
    ironclaw_turns::LoopResultRef,
) {
    let states = Arc::new(ironclaw_turns::test_support::in_memory_turn_state_store());
    let coordinator = Arc::new(DefaultTurnCoordinator::new(states.clone()));
    let actor = TurnActor::new(owner.clone());
    parent.run_id = submit_parent(coordinator.as_ref(), parent.scope.clone(), actor.clone()).await;
    parent.actor = Some(actor.clone());
    let runner_id = ironclaw_turns::TurnRunnerId::new();
    let lease_token = ironclaw_turns::TurnLeaseToken::new();
    states
        .claim_next_run(ClaimRunRequest {
            runner_id,
            lease_token,
            scope_filter: None,
        })
        .await
        .unwrap()
        .unwrap();
    let gate = GateRef::new("gate:full-result").unwrap();
    states
        .block_run(BlockRunRequest {
            run_id: parent.run_id,
            runner_id,
            lease_token,
            checkpoint_id: ironclaw_turns::TurnCheckpointId::new(),
            state_ref: ironclaw_turns::run_profile::LoopCheckpointStateRef::new(
                "checkpoint:full-result",
            )
            .unwrap(),
            reason: ironclaw_turns::BlockedReason::AwaitDependentRun {
                gate_ref: gate.clone(),
            },
        })
        .await
        .unwrap();
    let mut child_scope = parent.scope.clone();
    child_scope.thread_id = ThreadId::new("full-result-child").unwrap();
    let ironclaw_turns::SubmitTurnResponse::Accepted {
        run_id: child_id, ..
    } = coordinator
        .submit_child_run(SubmitChildRunRequest {
            parent_scope: parent.scope.clone(),
            parent_run_id: parent.run_id,
            child_scope: child_scope.clone(),
            actor,
            accepted_message_ref: AcceptedMessageRef::new("msg:full-child").unwrap(),
            source_binding_ref: ironclaw_turns::SourceBindingRef::new("source:full-child").unwrap(),
            reply_target_binding_ref: ReplyTargetBindingRef::new("reply:full-child").unwrap(),
            requested_run_profile: None,
            idempotency_key: ironclaw_turns::IdempotencyKey::new("idem:full-child").unwrap(),
            received_at: chrono::Utc::now(),
            requested_run_id: None,
            spawn_tree_descendant_cap: 16,
        })
        .await
        .unwrap();
    let scope = local_dev_thread_scope_for_run(parent, owner).unwrap();
    threads
        .ensure_thread(EnsureThreadRequest {
            scope: scope.clone(),
            thread_id: Some(child_scope.thread_id.clone()),
            created_by_actor_id: owner.as_str().into(),
            title: None,
            metadata_json: None,
        })
        .await
        .unwrap();
    threads
        .append_finalized_assistant_message(
            ironclaw_threads::AppendFinalizedAssistantMessageRequest {
                scope: scope.clone(),
                thread_id: child_scope.thread_id.clone(),
                turn_run_id: child_id.to_string(),
                content: ironclaw_threads::MessageContent::text(text),
            },
        )
        .await
        .unwrap();
    let input_ref = io
        .register_provider_tool_call_input(parent, &provider_tool_call(serde_json::json!({})))
        .await
        .unwrap();
    let capability_id = CapabilityId::new("builtin.spawn_subagent").unwrap();
    let write = io
        .write_capability_result(CapabilityResultWrite {
            run_context: parent,
            input_ref: &input_ref,
            invocation_id: InvocationId::new(),
            capability_id: &capability_id,
            output: serde_json::json!({"status":"spawned"}),
            display_preview: None,
            durable_persistence: DurablePersistence::Persist,
        })
        .await
        .unwrap();
    threads
        .append_tool_result_reference(AppendToolResultReferenceRequest {
            scope,
            thread_id: parent.thread_id.clone(),
            turn_run_id: parent.run_id.to_string(),
            result_ref: write.result_ref.as_str().into(),
            safe_summary: ToolResultSafeSummary::new("subagent spawned").unwrap(),
            provider_call: None,
            model_observation: None,
        })
        .await
        .unwrap();
    let store = FilesystemAwaitEdgeStore::new(edge_filesystem(backend));
    store
        .open(
            &child_scope,
            parent.run_id,
            child_id,
            AwaitEdge {
                child_scope: child_scope.clone(),
                child_thread_id: child_scope.thread_id.clone(),
                parent_thread_id: parent.thread_id.clone(),
                parent_run_context: parent.clone(),
                tree_root_run_id: parent.run_id,
                gate_ref: gate,
                source_binding_ref: ironclaw_turns::SourceBindingRef::new(
                    "subagent-source:full-result",
                )
                .unwrap(),
                reply_target_binding_ref: ReplyTargetBindingRef::new("subagent-reply:full-result")
                    .unwrap(),
                subagent_kind: ironclaw_loop_host::SubagentKindId::new("general").unwrap(),
                spawn_capability_id: capability_id,
                result_ref: write.result_ref.clone(),
                mode: ironclaw_loop_host::SpawnSubagentMode::Blocking,
                state: AwaitEdgeState::Open,
                terminal_kind: None,
                terminal_byte_len: None,
                terminal_reason: None,
                reservation_release: ReservationReleaseState::Unclaimed,
                created_at: chrono::Utc::now(),
                deadline: None,
                settled_at: None,
            },
        )
        .await
        .unwrap();
    let runner_id = ironclaw_turns::TurnRunnerId::new();
    let lease_token = ironclaw_turns::TurnLeaseToken::new();
    let claimed = states
        .claim_next_run(ClaimRunRequest {
            runner_id,
            lease_token,
            scope_filter: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.state.run_id, child_id);
    states
        .complete_run(CompleteRunRequest {
            run_id: child_id,
            runner_id,
            lease_token,
        })
        .await
        .unwrap();
    (coordinator, states, child_scope, write.result_ref)
}

async fn read_chunk(
    port: &dyn ironclaw_turns::run_profile::LoopCapabilityPort,
    reference: &str,
    offset: u64,
    max_bytes: usize,
) -> ironclaw_host_api::Outcome {
    let candidate = port
        .register_provider_tool_call(RegisterProviderToolCallRequest::new(
            provider_tool_call_with_name(
                "builtin__result_read",
                serde_json::json!({
                    "result_ref":reference, "offset":offset, "max_bytes":max_bytes,
                }),
            ),
        ))
        .await
        .unwrap();
    match port
        .invoke_capability(invocation_for_candidate(&candidate))
        .await
        .unwrap()
    {
        Resolution::Done(done) => done,
        other => panic!("expected bounded read outcome, got {other:?}"),
    }
}

// 覆盖：TianQuan TEST-SPEC §4 AE-L2-118，真实结果settler→writer→分页reader与恢复。
#[tokio::test]
async fn completed_child_result_survives_writer_recovery_and_bounded_reader() {
    let dir = tempfile::tempdir().unwrap();
    let services = crate::build_reborn_services(crate::RebornBuildInput::local_dev(
        "full-child-result",
        dir.path().join("local-dev"),
    ))
    .await
    .unwrap();
    let local = services.local_runtime.as_ref().unwrap();
    let owner = UserId::new("full-result-owner").unwrap();
    let mut parent = run_context_with_scope(TurnScope::new_with_owner(
        TenantId::new("full-result-tenant").unwrap(),
        Some(AgentId::new("full-result-agent").unwrap()),
        None,
        ThreadId::new("full-result-parent").unwrap(),
        Some(owner.clone()),
    ))
    .await;
    let backend = Arc::new(InMemoryBackend::new());
    let threads: Arc<dyn SessionThreadService> = Arc::new(FilesystemSessionThreadService::new(
        scoped_thread_filesystem(backend.clone()),
    ));
    ensure_thread_for_run(threads.as_ref(), &parent, &owner).await;
    let io = Arc::new(StagedCapabilityIo::new_with_durable_previews(
        Arc::new(CapabilityDisplayPreviewStore::default()),
        threads.clone(),
        owner.clone(),
    ));
    let text = serde_json::json!({"body":"河港钥匙\n🗝".repeat(1600),"tail":"urn:complete-tail"})
        .to_string();
    let (coordinator, states, child_scope, reference) =
        prepare_blocked_child(threads, io, backend.clone(), &mut parent, &owner, &text).await;
    let threads: Arc<dyn SessionThreadService> = Arc::new(FilesystemSessionThreadService::new(
        scoped_thread_filesystem(backend.clone()),
    ));
    let recovered_io = Arc::new(StagedCapabilityIo::new_with_durable_previews(
        Arc::new(CapabilityDisplayPreviewStore::default()),
        threads.clone(),
        owner.clone(),
    ));
    let edge_fs = edge_filesystem(backend.clone());
    let resolver = Arc::new(AwaitEdgeResolver::new_unbound(
        Arc::new(FilesystemAwaitEdgeStore::new(edge_fs.clone())),
        Arc::new(InMemoryBoundedSubagentGoalStore::new()),
        states,
        recovered_io.clone(),
        threads.clone(),
    ));
    resolver.bind_coordinator(coordinator.clone()).unwrap();
    let report =
        run_boot_recovery(resolver, edge_fs, Arc::new(tokio::sync::Semaphore::new(4))).await;
    assert_eq!(report.resumed, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(
        coordinator
            .get_run_state(ironclaw_turns::GetRunStateRequest {
                scope: parent.scope.clone(),
                run_id: parent.run_id,
            })
            .await
            .unwrap()
            .status,
        TurnStatus::Queued
    );
    let history = threads
        .list_thread_history(ThreadHistoryRequest {
            scope: local_dev_thread_scope_for_run(&parent, &owner).unwrap(),
            thread_id: parent.thread_id.clone(),
        })
        .await
        .unwrap();
    let summary = history
        .messages
        .iter()
        .find(|m| {
            m.tool_result_ref.as_deref() == Some(reference.as_str())
                && m.kind == MessageKind::ToolResultReference
        })
        .unwrap();
    let envelope = ironclaw_threads::ToolResultReferenceEnvelope::from_json_str(
        summary.content.as_ref().unwrap(),
    )
    .unwrap();
    assert!(envelope.safe_summary.as_str().len() <= 512);
    assert!(envelope.safe_summary.as_str().contains("Untrusted"));
    drop(recovered_io);
    drop(threads);
    let threads: Arc<dyn SessionThreadService> = Arc::new(FilesystemSessionThreadService::new(
        scoped_thread_filesystem(backend),
    ));
    let io = Arc::new(StagedCapabilityIo::new_with_durable_previews(
        Arc::new(CapabilityDisplayPreviewStore::default()),
        threads.clone(),
        owner.clone(),
    ));
    assert!(io.result_output(reference.as_str()).unwrap().is_none());
    let factory = RefreshingLoopCapabilityPortFactory {
        provider_input_store: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        runtime: services.host_runtime.clone().unwrap(),
        fallback_user_id: owner.clone(),
        policy: local.capability_policy.clone(),
        workspace_mounts: local.workspace_mounts.clone(),
        memory_mounts: local.memory_mounts.clone(),
        system_extensions_lifecycle_mounts: local.system_extensions_lifecycle_mounts.clone(),
        extension_surface_source: ExtensionCapabilitySurfaceSource::default(),
        input_resolver: io.clone(),
        result_writer: io.clone(),
        milestone_sink: Arc::new(InMemoryLoopHostMilestoneSink::default()),
        skill_activation_source: None,
        project_service: local.project_service.clone(),
        thread_service: threads.clone(),
        trajectory_observer: None,
        outbound_preferences_facade: None,
        outbound_delivery_target_set_requires_approval: false,
        approval_settings: Arc::new(
            crate::profile_approval_authorization::EmptyApprovalSettingsProvider,
        ),
        approval_requests: local.approval_requests.clone(),
        capability_leases: local.capability_leases.clone(),
        gate_record_store: Arc::new(ironclaw_run_state::FilesystemGateRecordStore::new(
            crate::wrap_scoped(local.extension_filesystem.clone()),
        )),
        replay_payload_store: Arc::new(ironclaw_capabilities::FilesystemReplayPayloadStore::new(
            crate::wrap_scoped(local.extension_filesystem.clone()),
        )),
        external_tool_catalog: Arc::new(ironclaw_turns::InMemoryExternalToolCatalog::new()),
    };
    let port = factory.create_capability_port(&parent).await.unwrap();
    let mut offset = 0;
    let mut full = String::new();
    let mut completed = false;
    for _ in 0..256 {
        let done = read_chunk(port.as_ref(), reference.as_str(), offset, 1021).await;
        assert!(done.verdict.error_kind().is_none(), "{:?}", done);
        let chunk = io
            .result_output(&completed_loop_result_ref(&done))
            .unwrap()
            .unwrap();
        let content = chunk["content"].as_str().unwrap();
        assert!(!content.is_empty());
        assert!(content.len() <= 1021);
        full.push_str(content);
        if let Some(next) = chunk["next_offset"].as_u64() {
            assert_eq!(next, offset + content.len() as u64);
            offset = next;
        } else {
            completed = true;
            break;
        }
    }
    assert!(completed, "finite read budget exhausted");
    let payload: serde_json::Value = serde_json::from_str(&full).unwrap();
    assert!(payload["final_text"].as_str().unwrap().contains(&text));
    assert!(
        payload["final_text"]
            .as_str()
            .unwrap()
            .contains("urn:complete-tail")
    );
    assert_eq!(payload["output_available"], true);
    for (key, value) in [
        ("thread", "other-parent"),
        ("tenant", "other-tenant"),
        ("owner", "other-owner"),
    ] {
        let mut wrong = parent.clone();
        match key {
            "thread" => wrong.thread_id = ThreadId::new(value).unwrap(),
            "tenant" => wrong.scope.tenant_id = TenantId::new(value).unwrap(),
            _ => {
                let user = UserId::new(value).unwrap();
                wrong.scope.thread_owner =
                    ironclaw_turns::scope::TurnThreadOwner::explicit(Some(user.clone()));
                wrong.actor = Some(TurnActor::new(user));
            }
        }
        wrong.scope.thread_id = wrong.thread_id.clone();
        let port = factory.create_capability_port(&wrong).await.unwrap();
        assert_eq!(
            read_chunk(port.as_ref(), reference.as_str(), 0, 1021)
                .await
                .verdict
                .error_kind(),
            Some(&FailureKind::InvalidInput)
        );
    }
    for bytes in [0, ironclaw_threads::TOOL_RESULT_RECORD_READ_MAX_BYTES + 1] {
        assert_eq!(
            read_chunk(port.as_ref(), reference.as_str(), 0, bytes)
                .await
                .verdict
                .error_kind(),
            Some(&FailureKind::InvalidInput)
        );
    }
    let mut other_run = parent.clone();
    other_run.run_id = TurnRunId::new();
    assert_eq!(
        io.update_capability_result(&other_run, &reference, serde_json::json!({}))
            .await
            .unwrap_err()
            .kind,
        AgentLoopHostErrorKind::ScopeMismatch
    );
    let oversized = serde_json::Value::String("x".repeat(LOCAL_DEV_DURABLE_TOOL_RESULT_MAX_BYTES));
    assert_eq!(
        io.update_capability_result(&parent, &reference, oversized)
            .await
            .unwrap_err()
            .kind,
        AgentLoopHostErrorKind::BudgetExceeded
    );
    let again = read_chunk(port.as_ref(), reference.as_str(), 0, 1021).await;
    let chunk = io
        .result_output(&completed_loop_result_ref(&again))
        .unwrap()
        .unwrap();
    assert_eq!(
        chunk["content"].as_str().unwrap(),
        &full[..chunk["content"].as_str().unwrap().len()]
    );
    let mut next_turn = parent.clone();
    next_turn.run_id = TurnRunId::new();
    let next_port = factory.create_capability_port(&next_turn).await.unwrap();
    assert!(
        read_chunk(next_port.as_ref(), reference.as_str(), 0, 1021)
            .await
            .verdict
            .error_kind()
            .is_none()
    );
    assert_ne!(child_scope.thread_id, parent.thread_id);
}
