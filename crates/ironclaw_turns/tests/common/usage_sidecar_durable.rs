//! Mechanical cfg(test) usage only. Reuse the original coordinator/row-store harness.
//! A fresh store instance over the same InMemoryBackend is a cold cache read,
//! not a real process/PG/libSQL restart or a normal provider qualification.
use super::*;

fn v1_fixture_usage() -> LoopModelUsage {
    LoopModelUsage {
        normalization_version: Some(1),
        normalized_input_tokens: Some(109),
        cache_details_observed: Some(true),
        input_tokens: 2,
        output_tokens: 3,
        cache_read_input_tokens: 7,
        cache_creation_input_tokens: 100,
    }
}

async fn normal_terminal_then_cold_reopen(
    label: &str,
    usage: LoopModelUsage,
) -> (ironclaw_turns::TurnRunRecord, TurnLifecycleEvent) {
    let filesystem = in_memory_turns_filesystem();
    let store = Arc::new(FilesystemTurnStateRowStore::new(filesystem.clone()));
    let coordinator = DefaultTurnCoordinator::new(store.clone());
    let thread = format!("thread-sidecar-{label}");
    let run_id = accepted_run_id(
        &coordinator
            .submit_turn(submit_request(&thread, &format!("idem-sidecar-{label}")))
            .await
            .expect("normal fixture admission"),
    );
    let runner_id = TurnRunnerId::new();
    let lease_token = TurnLeaseToken::new();
    store
        .claim_next_run(ClaimRunRequest {
            runner_id,
            lease_token,
            scope_filter: Some(scope(&thread)),
        })
        .await
        .expect("normal claim")
        .expect("original run");
    let terminal = store
        .apply_validated_loop_exit(ApplyValidatedLoopExitRequest {
            model_usage: Some(usage),
            run_id,
            runner_id,
            lease_token,
            mapping: completed_mapping(),
        })
        .await
        .expect("normal validated terminal transition");
    assert_eq!(terminal.status, TurnStatus::Completed);
    assert_eq!(terminal.model_usage, Some(usage));
    store.drain().await.expect("original write-behind drain");
    drop(coordinator);
    drop(store);

    // No persistence_snapshot seeding or edited rows: load the original durable
    // row/journal bytes via a fresh store instance without the original cache.
    let cold = FilesystemTurnStateRowStore::new(filesystem);
    let snapshot = cold
        .persistence_snapshot()
        .await
        .expect("cold durable read");
    let rows = snapshot
        .runs
        .iter()
        .filter(|row| row.run_id == run_id)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);
    let events = snapshot
        .events
        .iter()
        .filter(|event| event.run_id == run_id && event.kind == TurnEventKind::Completed)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(rows[0].status, TurnStatus::Completed);
    assert_eq!(rows[0].model_usage, Some(usage));
    assert_eq!(events[0].model_usage, Some(usage));
    assert_eq!(events[0].scope, rows[0].scope);
    assert_eq!(events[0].status, rows[0].status);
    (rows[0].clone(), events[0].clone())
}

#[tokio::test]
async fn v1_sidecar_raw_counters_and_terminal_event_survive_normal_row_store_cold_read() {
    let usage = v1_fixture_usage();
    let (row, event) = normal_terminal_then_cold_reopen("v1", usage).await;
    let expected = serde_json::json!({
        "normalization_version":1,"normalized_input_tokens":109,"cache_details_observed":true,
        "input_tokens":2,"output_tokens":3,
        "cache_read_input_tokens":7,"cache_creation_input_tokens":100
    });
    assert_eq!(
        serde_json::to_value(row.model_usage.unwrap()).unwrap(),
        expected
    );
    assert_eq!(
        serde_json::to_value(event.model_usage.unwrap()).unwrap(),
        expected
    );
    let bytes = serde_json::to_vec(&row).unwrap();
    let decoded: ironclaw_turns::TurnRunRecord = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded, row);
    let bytes = serde_json::to_vec(&event).unwrap();
    let decoded: TurnLifecycleEvent = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded, event);
}

#[tokio::test]
async fn legacy_json_without_sidecars_stays_unknown_after_normal_durable_cold_read() {
    // An actual historical JSON shape, not Default() promoted to a new version.
    let usage: LoopModelUsage = serde_json::from_value(serde_json::json!({
        "input_tokens":2,"output_tokens":3,
        "cache_read_input_tokens":7,"cache_creation_input_tokens":100
    }))
    .unwrap();
    assert_eq!(usage.normalization_version, None);
    assert_eq!(usage.normalized_input_tokens, None);
    let (row, event) = normal_terminal_then_cold_reopen("legacy", usage).await;
    for actual in [row.model_usage.unwrap(), event.model_usage.unwrap()] {
        assert_eq!(actual, usage);
        let value = serde_json::to_value(actual).unwrap();
        assert!(value.get("normalization_version").is_none());
        assert!(value.get("normalized_input_tokens").is_none());
        assert!(value.get("cache_details_observed").is_none());
    }
}

#[tokio::test]
async fn legacy_some_zero_and_unsupported_version_are_preserved_not_promoted_by_cold_read() {
    for (label, value) in [
        (
            "legacy-zero",
            serde_json::json!({"input_tokens":0,"output_tokens":0}),
        ),
        (
            "unsupported",
            serde_json::json!({
                "normalization_version":0,"normalized_input_tokens":0,
                "input_tokens":0,"output_tokens":0
            }),
        ),
    ] {
        let usage: LoopModelUsage = serde_json::from_value(value).unwrap();
        let (row, event) = normal_terminal_then_cold_reopen(label, usage).await;
        for actual in [row.model_usage.unwrap(), event.model_usage.unwrap()] {
            assert_eq!(actual, usage);
            assert_ne!(actual.normalization_version, Some(1));
        }
    }
    // Carrier preservation is not provenance. The real RunUsagePrefix::ingest
    // consumer owns missing/version/mismatching-event rejection tests.
}
