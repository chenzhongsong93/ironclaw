use super::*;
use ironclaw_host_api::{AgentId, ProjectId, TenantId, ThreadId};
fn scope(thread: &str) -> TurnScope {
    TurnScope::new(
        TenantId::new("tenant-test").unwrap(),
        Some(AgentId::new("agent-test").unwrap()),
        Some(ProjectId::new("project-test").unwrap()),
        ThreadId::new(thread).unwrap(),
    )
}
fn record(
    id: TurnRunId,
    parent: Option<TurnRunId>,
    root: Option<TurnRunId>,
    depth: u32,
) -> TurnRunRecord {
    // Same complete typed record shape as the production store contract fixture.
    serde_json::from_value(serde_json::json!({
        "run_id": id, "turn_id": crate::TurnId::new(), "scope": scope(&format!("thread-{id}")),
        "accepted_message_ref": "accepted-test", "source_binding_ref": "source-test", "reply_target_binding_ref": "reply-test",
        "status": TurnStatus::Completed, "profile": {"id":"default","version":1,"allow_steering":false,"auto_queue_followups":false},
        "model_usage": LoopModelUsage {normalization_version:Some(1),normalized_input_tokens:Some(7),input_tokens: 7, output_tokens: 3, ..Default::default()},
        "checkpoint_id":null,"gate_ref":null,"failure":null,"event_cursor":19,"runner_id":null,"lease_token":null,
        "lease_expires_at":null,"last_heartbeat_at":null,"claim_count":1,"received_at":"2026-10-21T00:00:00Z",
        "parent_run_id":parent,"subagent_depth":depth,"spawn_tree_root_run_id":root
    })).unwrap()
}

// Drive the ACTUAL typed decoder through original birth then terminal delta.
fn ingest(
    p: &mut RunUsagePrefix,
    seq: u64,
    rows: Vec<TurnRunRecord>,
    deleted: bool,
) -> Result<(), TurnError> {
    let actual = seq * 2;
    let new: Vec<_> = rows
        .iter()
        .filter(|r| !p.rows.contains_key(&r.run_id))
        .cloned()
        .collect();
    if !new.is_empty() {
        let mut births = Vec::new();
        let mut events = Vec::new();
        for mut r in new {
            r.status = TurnStatus::Queued;
            r.claim_count = 0;
            r.model_usage = None;
            r.event_cursor = EventCursor(1);
            events.push(serde_json::from_value(serde_json::json!({"cursor":1,"scope":r.scope.clone(),"run_id":r.run_id,"status":TurnStatus::Queued,"kind":TurnEventKind::Submitted})).unwrap());
            births.push(r);
        }
        p.ingest(actual - 1, births, &events, false)?;
    }
    let events = terminal_events(&rows);
    p.ingest(actual, rows, &events, deleted)
}

fn terminal_events(rows: &[TurnRunRecord]) -> Vec<TurnLifecycleEvent> {
    rows.iter().filter(|r|r.status.is_terminal()).map(|r| {
  let kind=match r.status {TurnStatus::Completed=>TurnEventKind::Completed,TurnStatus::Failed=>TurnEventKind::Failed,TurnStatus::Cancelled=>TurnEventKind::Cancelled,_=>unreachable!()};
  serde_json::from_value(serde_json::json!({"cursor":r.event_cursor,"scope":r.scope.clone(),"run_id":r.run_id,"status":r.status,"kind":kind,"model_usage":r.model_usage})).unwrap()
 }).collect()
}

struct Access {
    denied: Option<TurnScope>,
}
#[async_trait]
impl RunTreeUsageAccess for Access {
    async fn authorize_exact_scope(&self, s: &TurnScope) -> Result<(), TurnError> {
        if self.denied.as_ref() == Some(s) {
            Err(TurnError::ScopeNotFound)
        } else {
            Ok(())
        }
    }
}
fn session(count: usize, sequence: u64) -> RunTreeUsageSession {
    let root = TurnRunId::new();
    let r = record(root, None, None, 0);
    let sc = r.scope.clone();
    let mut rows = vec![r];
    for _ in 1..count {
        rows.push(record(TurnRunId::new(), Some(root), Some(root), 1));
    }
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, sequence, rows, false).unwrap();
    p.finish(sc, root, sequence * 2, "test-gen".into()).unwrap()
}
#[test]
fn legal_global_sequence_gaps_are_not_retention_proof() {
    let root = TurnRunId::new();
    let r = record(root, None, None, 0);
    let sc = r.scope.clone();
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 4, vec![r.clone()], false).unwrap();
    ingest(&mut p, 19, vec![r], false).unwrap();
    assert!(p.finish(sc, root, 38, "test-gen".into()).is_ok());
}
#[test]
fn duplicate_or_backwards_sequence_rejected() {
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 8, vec![], false).unwrap();
    assert!(ingest(&mut p, 8, vec![], false).is_err());
    assert!(ingest(&mut p, 7, vec![], false).is_err());
}
#[test]
fn tombstone_is_not_silently_ignored() {
    assert!(ingest(&mut RunUsagePrefix::new(), 1, vec![], true).is_err());
}
#[test]
fn same_run_changed_lineage_rejected() {
    let id = TurnRunId::new();
    let r = record(id, None, None, 0);
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 1, vec![r.clone()], false).unwrap();
    let mut x = r;
    x.parent_run_id = Some(TurnRunId::new());
    assert!(ingest(&mut p, 2, vec![x], false).is_err());
}
#[test]
fn same_run_changed_terminal_usage_rejected() {
    let id = TurnRunId::new();
    let r = record(id, None, None, 0);
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 1, vec![r.clone()], false).unwrap();
    let mut x = r;
    x.model_usage.as_mut().unwrap().input_tokens += 1;
    assert!(ingest(&mut p, 2, vec![x], false).is_err());
}
#[test]
fn first_known_terminal_usage_source_stays_stable_across_h() {
    let id = TurnRunId::new();
    let r = record(id, None, None, 0);
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 9, vec![r.clone()], false).unwrap();
    let first = p.provenance[&id].clone();
    ingest(&mut p, 29, vec![r], false).unwrap();
    assert_eq!(first, p.provenance[&id]);
    assert_eq!(first.first_terminal_usage_journal_sequence, 18);
}
#[test]
fn child_cannot_be_disguised_as_root() {
    let root = TurnRunId::new();
    let child = TurnRunId::new();
    let r = record(child, Some(root), Some(root), 1);
    let sc = r.scope.clone();
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 1, vec![r], false).unwrap();
    assert!(p.finish(sc, child, 2, "test-gen".into()).is_err());
}
#[test]
fn orphan_declared_under_root_rejected() {
    let root = TurnRunId::new();
    let r = record(root, None, None, 0);
    let sc = r.scope.clone();
    let child = record(TurnRunId::new(), Some(TurnRunId::new()), Some(root), 2);
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 1, vec![r, child], false).unwrap();
    assert!(p.finish(sc, root, 2, "test-gen".into()).is_err());
}
#[test]
fn missing_child_root_ref_is_not_filtered() {
    let root = TurnRunId::new();
    let r = record(root, None, None, 0);
    let sc = r.scope.clone();
    let child = record(TurnRunId::new(), Some(root), None, 1);
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 1, vec![r, child], false).unwrap();
    assert!(p.finish(sc, root, 2, "test-gen".into()).is_err());
}
#[test]
fn duplicate_in_same_delta_rejected() {
    let r = record(TurnRunId::new(), None, None, 0);
    assert!(ingest(&mut RunUsagePrefix::new(), 1, vec![r.clone(), r], false).is_err());
}
#[tokio::test]
async fn pagination_is_complete_only_after_every_original_page() {
    let mut s = session(130, 21);
    let sc = s.root_scope.clone();
    let allow = Access { denied: None };
    let a = s.read_page(&allow, &sc, None, 128).await.unwrap();
    assert!(!a.pages_complete_at_sequence);
    assert_eq!(a.members.len(), 128);
    let b = s
        .read_page(&allow, &sc, a.next_cursor.as_deref(), 128)
        .await
        .unwrap();
    assert_eq!(b.members.len(), 2);
    assert!(b.pages_complete_at_sequence);
    assert!(!b.permanent_closed);
    assert!(s.read_page(&allow, &sc, None, 128).await.is_err());
}
#[tokio::test]
async fn forged_cross_session_and_skip_cursor_rejected() {
    let mut s = session(130, 21);
    let sc = s.root_scope.clone();
    let allow = Access { denied: None };
    assert!(
        s.read_page(&allow, &sc, Some("fake:root:21:128"), 128)
            .await
            .is_err()
    );
    let mut other = session(130, 21);
    let osc = other.root_scope.clone();
    let a = other.read_page(&allow, &osc, None, 128).await.unwrap();
    assert!(
        s.read_page(&allow, &sc, a.next_cursor.as_deref(), 128)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn foreign_owner_scope_rejected_before_any_rows() {
    let mut s = session(2, 1);
    let wrong = scope("foreign");
    assert!(
        s.read_page(&Access { denied: None }, &wrong, None, 2)
            .await
            .is_err()
    );
    assert_eq!(s.next, 0);
}
#[tokio::test]
async fn every_historical_member_acl_checked_every_page() {
    let mut s = session(130, 1);
    let sc = s.root_scope.clone();
    let a = s
        .read_page(&Access { denied: None }, &sc, None, 128)
        .await
        .unwrap();
    let denied = s.rows[0].0.clone();
    assert!(
        s.read_page(
            &Access {
                denied: Some(denied)
            },
            &sc,
            a.next_cursor.as_deref(),
            128
        )
        .await
        .is_err()
    );
    assert_eq!(s.next, 128);
}
#[tokio::test]
async fn unknown_usage_is_never_zero() {
    let root = TurnRunId::new();
    let mut r = record(root, None, None, 0);
    r.model_usage = None;
    let sc = r.scope.clone();
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 7, vec![r], false).unwrap();
    let mut s = p.finish(sc.clone(), root, 14, "test-gen".into()).unwrap();
    let page = s
        .read_page(&Access { denied: None }, &sc, None, 128)
        .await
        .unwrap();
    assert!(page.pages_complete_at_sequence);
    assert!(!page.usage_known_at_sequence);
    assert!(page.members[0].usage.is_none());
}
#[tokio::test]
async fn legal_child_retry_new_id_preserves_old_root_without_seal() {
    let root = TurnRunId::new();
    let r = record(root, None, None, 0);
    let sc = r.scope.clone();
    let child = record(TurnRunId::new(), Some(root), Some(root), 1);
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 11, vec![r, child.clone()], false).unwrap();
    let mut retry = child.clone();
    retry.run_id = TurnRunId::new();
    ingest(&mut p, 31, vec![retry], false).unwrap();
    let mut s = p.finish(sc.clone(), root, 62, "test-gen".into()).unwrap();
    let page = s
        .read_page(&Access { denied: None }, &sc, None, 128)
        .await
        .unwrap();
    assert_eq!(page.members.len(), 3);
    assert!(!page.permanent_closed);
}
#[tokio::test]
async fn page_limits_and_invalid_cursor_do_not_consume_state() {
    let mut s = session(2, 1);
    let sc = s.root_scope.clone();
    assert!(
        s.read_page(&Access { denied: None }, &sc, None, 0)
            .await
            .is_err()
    );
    assert!(
        s.read_page(&Access { denied: None }, &sc, None, 129)
            .await
            .is_err()
    );
    assert_eq!(s.next, 0);
}

#[test]
fn retained_late_root_update_cannot_forge_current_incarnation_birth() {
    let id = TurnRunId::new();
    let r = record(id, None, None, 0);
    let sc = r.scope.clone();
    let mut p = RunUsagePrefix::new();
    assert!(p.ingest(94, vec![r], &[], false).is_err());
    let _ = sc;
}
#[test]
fn birth_with_wrong_scope_or_cursor_is_not_evidence() {
    let id = TurnRunId::new();
    let mut r = record(id, None, None, 0);
    r.status = TurnStatus::Queued;
    r.claim_count = 0;
    let sc = r.scope.clone();
    let e=serde_json::from_value(serde_json::json!({"cursor":999,"scope":sc.clone(),"run_id":id,"status":TurnStatus::Queued,"kind":TurnEventKind::Submitted})).unwrap();
    let mut p = RunUsagePrefix::new();
    p.ingest(7, vec![r], &[e], false).unwrap();
    assert!(p.finish(sc, id, 7, "gen".into()).is_err());
}

#[test]
fn resumed_new_run_birth_is_accepted_without_reopening_old_run() {
    let root = TurnRunId::new();
    let r = record(root, None, None, 0);
    let sc = r.scope.clone();
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 1, vec![r], false).unwrap();
    let mut retry = record(TurnRunId::new(), Some(root), Some(root), 1);
    let final_record = retry.clone();
    retry.status = TurnStatus::Queued;
    retry.claim_count = 0;
    retry.model_usage = None;
    let event=serde_json::from_value(serde_json::json!({"cursor":retry.event_cursor,"scope":retry.scope.clone(),"run_id":retry.run_id,"status":TurnStatus::Queued,"kind":TurnEventKind::Resumed})).unwrap();
    p.ingest(3, vec![retry], &[event], false).unwrap();
    let events = terminal_events(&[final_record.clone()]);
    p.ingest(4, vec![final_record], &events, false).unwrap();
    assert!(p.finish(sc, root, 4, "gen".into()).is_ok());
}
#[tokio::test]
async fn response_byte_budget_rejection_does_not_advance_cursor() {
    let mut s = session(1, 1);
    s.rows[0]
        .1
        .usage_provenance
        .as_mut()
        .unwrap()
        .usage_record_digest = "x".repeat(TREE_MAX_PAGE_BYTES);
    let sc = s.root_scope.clone();
    assert!(
        s.read_page(&Access { denied: None }, &sc, None, 1)
            .await
            .is_err()
    );
    assert_eq!(s.next, 0);
}
#[test]
fn historical_member_bound_is_not_concurrent_reservation_cap() {
    let root = TurnRunId::new();
    let r = record(root, None, None, 0);
    let sc = r.scope.clone();
    let mut rows = vec![r];
    for _ in 0..TREE_MAX_MEMBERS {
        rows.push(record(TurnRunId::new(), Some(root), Some(root), 1));
    }
    let mut p = RunUsagePrefix::new();
    ingest(&mut p, 1, rows, false).unwrap();
    assert!(p.finish(sc, root, 2, "gen".into()).is_err());
}

#[test]
fn terminal_upsert_without_original_event_cannot_mint_usage_provenance() {
    let id = TurnRunId::new();
    let mut r = record(id, None, None, 0);
    r.status = TurnStatus::Queued;
    r.claim_count = 0;
    r.model_usage = None;
    let birth=serde_json::from_value(serde_json::json!({"cursor":r.event_cursor,"scope":r.scope.clone(),"run_id":id,"status":TurnStatus::Queued,"kind":TurnEventKind::Submitted})).unwrap();
    let mut p = RunUsagePrefix::new();
    p.ingest(1, vec![r], &[birth], false).unwrap();
    assert!(
        p.ingest(9, vec![record(id, None, None, 0)], &[], false)
            .is_err()
    );
    assert!(!p.provenance.contains_key(&id));
}
#[test]
fn terminal_wrong_cursor_scope_kind_or_usage_is_rejected() {
    for changed in 0..6 {
        let id = TurnRunId::new();
        let r = record(id, None, None, 0);
        let mut events = terminal_events(&[r.clone()]);
        match changed {
            0 => events[0].cursor = EventCursor(999),
            1 => events[0].scope = scope("other-thread"),
            2 => events[0].kind = TurnEventKind::RunnerHeartbeat,
            3 => events[0].model_usage.as_mut().unwrap().output_tokens += 1,
            4 => {
                events[0]
                    .model_usage
                    .as_mut()
                    .unwrap()
                    .normalized_input_tokens = Some(999)
            }
            _ => {
                events[0]
                    .model_usage
                    .as_mut()
                    .unwrap()
                    .normalization_version = None
            }
        };
        assert!(
            RunUsagePrefix::new()
                .ingest(19, vec![r], &events, false)
                .is_err()
        );
    }
}
#[test]
fn failed_and_cancelled_real_terminal_tokens_are_retained() {
    for status in [TurnStatus::Failed, TurnStatus::Cancelled] {
        let id = TurnRunId::new();
        let mut r = record(id, None, None, 0);
        r.status = status;
        let events = terminal_events(&[r.clone()]);
        let mut p = RunUsagePrefix::new();
        p.ingest(19, vec![r], &events, false).unwrap();
        assert!(p.provenance.contains_key(&id));
        assert_eq!(p.rows[&id].model_usage.unwrap().input_tokens, 7);
    }
}

#[tokio::test]
async fn legacy_zero_or_missing_normalization_has_no_trusted_provenance() {
    let root = TurnRunId::new();
    let mut row = record(root, None, None, 0);
    row.model_usage.as_mut().unwrap().normalization_version = None;
    row.model_usage.as_mut().unwrap().normalized_input_tokens = None;
    let sc = row.scope.clone();
    let mut prefix = RunUsagePrefix::new();
    ingest(&mut prefix, 3, vec![row], false).unwrap();
    let mut original = prefix
        .finish(sc.clone(), root, 6, "legacy-unknown".into())
        .unwrap();
    let page = original
        .read_page(&Access { denied: None }, &sc, None, 128)
        .await
        .unwrap();
    assert!(!page.usage_known_at_sequence);
    assert!(page.members[0].usage.is_some());
    assert!(page.members[0].usage_provenance.is_none());
}
