use ironclaw_filesystem::ScopedFilesystem;
use ironclaw_host_runtime::{TODO_READ_CAPABILITY_ID, TODO_WRITE_CAPABILITY_ID, ThreadPlanTools};
use ironclaw_threads::plan::FilesystemThreadPlanStore;

use super::*;

fn todo_context(capability: &str, thread: &str) -> ExecutionContext {
    let mut context = execution_context([capability]);
    context.resource_scope.thread_id = Some(ThreadId::new(thread).unwrap());
    context
}

fn todo_runtime() -> impl HostRuntime {
    let backend = Arc::new(InMemoryBackend::new());
    let store = Arc::new(FilesystemThreadPlanStore::new(Arc::new(
        ScopedFilesystem::new(backend.clone(), |_scope: &ResourceScope| {
            MountView::new(vec![MountGrant::new(
                MountAlias::new("/threads")?,
                VirtualPath::new("/threads")?,
                MountPermissions::read_write_list_delete(),
            )])
        }),
    )));
    let mut handlers =
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap();
    ThreadPlanTools::new(store)
        .insert_into(&mut handlers)
        .unwrap();
    HostRuntimeServices::new(
        Arc::new(registry()),
        backend,
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("todo-contract").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(handlers))
    .with_runtime_policy(local_dev_policy())
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

#[tokio::test]
async fn todo_caller_respects_authorized_subject_scope_with_a_delegated_actor() {
    let runtime = todo_runtime();
    let mut context = todo_context(TODO_WRITE_CAPABILITY_ID, "shared-subject-thread");
    context.authenticated_actor_user_id = Some(UserId::new("delegated-actor").unwrap());
    let created = invoke_with_context(
        &runtime,
        TODO_WRITE_CAPABILITY_ID,
        json!({"title":"", "steps":[]}),
        context,
    )
    .await
    .unwrap();
    assert_eq!(created["revision"], 1);
    assert_eq!(
        invoke_with_context(
            &runtime,
            TODO_READ_CAPABILITY_ID,
            json!({}),
            todo_context(TODO_READ_CAPABILITY_ID, "shared-subject-thread")
        )
        .await
        .unwrap(),
        created
    );
}

#[tokio::test]
async fn todo_caller_path_creates_updates_clears_and_isolates_child_thread() {
    let runtime = todo_runtime();
    let read = |thread| todo_context(TODO_READ_CAPABILITY_ID, thread);
    let write = || todo_context(TODO_WRITE_CAPABILITY_ID, "parent");
    assert!(
        invoke_with_context(&runtime, TODO_READ_CAPABILITY_ID, json!({}), read("parent"))
            .await
            .unwrap()
            .is_null()
    );
    let created = invoke_with_context(&runtime, TODO_WRITE_CAPABILITY_ID, json!({
        "title":"检查", "steps":[{"index":0,"title":"执行真实工具","status":"pending"}], "expected_revision":0
    }), write()).await.unwrap();
    assert_eq!(created["revision"], 1);
    assert_eq!(created["kind"], "plan_update");
    assert_eq!(
        invoke_with_context(&runtime, TODO_READ_CAPABILITY_ID, json!({}), read("parent"))
            .await
            .unwrap(),
        created
    );
    assert!(
        invoke_with_context(&runtime, TODO_READ_CAPABILITY_ID, json!({}), read("child"))
            .await
            .unwrap()
            .is_null()
    );
    let updated = invoke_with_context(&runtime, TODO_WRITE_CAPABILITY_ID, json!({
        "title":"检查", "steps":[{"index":0,"title":"执行真实工具","status":"completed"}], "expected_revision":1
    }), write()).await.unwrap();
    assert_eq!(updated["revision"], 2);
    let cleared = invoke_with_context(
        &runtime,
        TODO_WRITE_CAPABILITY_ID,
        json!({"title":"","steps":[],"expected_revision":2}),
        write(),
    )
    .await
    .unwrap();
    assert_eq!(cleared["revision"], 3);
    assert_eq!(cleared["steps"], json!([]));
}

#[tokio::test]
async fn todo_caller_path_rejects_scope_injection_and_missing_grant_without_writes() {
    let runtime = todo_runtime();
    let rejected = invoke_with_context(
        &runtime,
        TODO_WRITE_CAPABILITY_ID,
        json!({
            "title":"越界", "steps":[], "thread_id":"victim"
        }),
        todo_context(TODO_WRITE_CAPABILITY_ID, "parent"),
    )
    .await
    .unwrap_err();
    assert_eq!(rejected, RuntimeFailureKind::InvalidInput);
    let mut no_grant = execution_context(std::iter::empty::<&str>());
    no_grant.resource_scope.thread_id = Some(ThreadId::new("parent").unwrap());
    let rejected = invoke_with_context(
        &runtime,
        TODO_WRITE_CAPABILITY_ID,
        json!({"title":"","steps":[]}),
        no_grant,
    )
    .await
    .unwrap_err();
    assert_eq!(rejected, RuntimeFailureKind::Authorization);
    assert!(
        invoke_with_context(
            &runtime,
            TODO_READ_CAPABILITY_ID,
            json!({}),
            todo_context(TODO_READ_CAPABILITY_ID, "parent")
        )
        .await
        .unwrap()
        .is_null()
    );
}
