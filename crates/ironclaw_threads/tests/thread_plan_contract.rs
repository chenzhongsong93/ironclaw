use std::sync::Arc;

use ironclaw_filesystem::{
    CasExpectation, Entry, InMemoryBackend, RootFilesystem, ScopedFilesystem,
};
use ironclaw_host_api::{
    InvocationId, MountAlias, MountGrant, MountPermissions, MountView, ResourceScope, TenantId,
    ThreadId, UserId, VirtualPath,
};
use ironclaw_threads::plan::{
    FilesystemThreadPlanStore, ThreadPlanError, ThreadPlanStatus, ThreadPlanStep, ThreadPlanWrite,
};

fn scope() -> ResourceScope {
    ResourceScope {
        tenant_id: TenantId::new("plan-tenant").unwrap(),
        user_id: UserId::new("plan-user").unwrap(),
        agent_id: None,
        project_id: None,
        mission_id: None,
        thread_id: Some(ThreadId::new("plan-thread").unwrap()),
        invocation_id: InvocationId::new(),
    }
}

fn filesystem() -> Arc<ScopedFilesystem<InMemoryBackend>> {
    Arc::new(ScopedFilesystem::new(
        Arc::new(InMemoryBackend::new()),
        |_scope: &ResourceScope| {
            MountView::new(vec![MountGrant::new(
                MountAlias::new("/threads")?,
                VirtualPath::new("/threads")?,
                MountPermissions::read_write_list_delete(),
            )])
        },
    ))
}

fn write(status: ThreadPlanStatus) -> ThreadPlanWrite {
    ThreadPlanWrite {
        title: "校验计划".to_string(),
        steps: vec![ThreadPlanStep {
            index: 0,
            title: "核对真实执行结果".to_string(),
            status,
        }],
        expected_revision: None,
    }
}

#[tokio::test]
async fn actual_plan_create_update_clear_and_reopen_are_durable() {
    let fs = filesystem();
    let store = FilesystemThreadPlanStore::new(fs.clone());
    let scope = scope();
    assert!(store.read(&scope).await.unwrap().is_none());
    let created = store
        .update(&scope, write(ThreadPlanStatus::Pending), None)
        .await
        .unwrap();
    assert_eq!(created.revision, 1);
    assert_eq!(
        serde_json::to_value(&created).unwrap()["kind"],
        "plan_update"
    );
    let reopened = FilesystemThreadPlanStore::new(fs.clone());
    assert_eq!(reopened.read(&scope).await.unwrap(), Some(created));
    let updated = reopened
        .update(&scope, write(ThreadPlanStatus::Completed), None)
        .await
        .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(updated.steps[0].status, ThreadPlanStatus::Completed);
    let cleared = reopened
        .update(
            &scope,
            ThreadPlanWrite {
                title: String::new(),
                steps: vec![],
                expected_revision: Some(2),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(cleared.revision, 3);
    assert!(
        FilesystemThreadPlanStore::new(fs)
            .read(&scope)
            .await
            .unwrap()
            .unwrap()
            .steps
            .is_empty()
    );
}

#[tokio::test]
async fn plans_isolate_thread_user_tenant_and_ignore_invocation_identity() {
    let store = FilesystemThreadPlanStore::new(filesystem());
    let scope = scope();
    store
        .update(&scope, write(ThreadPlanStatus::InProgress), None)
        .await
        .unwrap();
    let mut next_turn = scope.clone();
    next_turn.invocation_id = InvocationId::new();
    assert!(store.read(&next_turn).await.unwrap().is_some());
    let mut sibling = scope.clone();
    sibling.thread_id = Some(ThreadId::new("child-thread").unwrap());
    assert!(store.read(&sibling).await.unwrap().is_none());
    sibling = scope.clone();
    sibling.user_id = UserId::new("other-user").unwrap();
    assert!(store.read(&sibling).await.unwrap().is_none());
    sibling = scope.clone();
    sibling.tenant_id = TenantId::new("other-tenant").unwrap();
    assert!(store.read(&sibling).await.unwrap().is_none());
    sibling = scope.clone();
    sibling.agent_id = Some(ironclaw_host_api::AgentId::new("other-agent").unwrap());
    assert!(store.read(&sibling).await.unwrap().is_none());
    sibling = scope.clone();
    sibling.project_id = Some(ironclaw_host_api::ProjectId::new("other-project").unwrap());
    assert!(store.read(&sibling).await.unwrap().is_none());
    sibling = scope.clone();
    sibling.mission_id = Some(ironclaw_host_api::MissionId::new("other-mission").unwrap());
    assert!(store.read(&sibling).await.unwrap().is_none());
    sibling.thread_id = None;
    assert!(matches!(
        store.read(&sibling).await,
        Err(ThreadPlanError::MissingThread)
    ));
}

#[tokio::test]
async fn stale_revision_and_invalid_steps_cannot_overwrite_existing_plan() {
    let store = FilesystemThreadPlanStore::new(filesystem());
    let scope = scope();
    let initial = store
        .update(&scope, write(ThreadPlanStatus::Pending), None)
        .await
        .unwrap();
    let mut stale = write(ThreadPlanStatus::Completed);
    stale.expected_revision = Some(0);
    assert!(matches!(
        store.update(&scope, stale, None).await,
        Err(ThreadPlanError::RevisionConflict)
    ));
    let mut invalid = write(ThreadPlanStatus::Pending);
    invalid.steps.push(invalid.steps[0].clone());
    assert!(matches!(
        store.update(&scope, invalid, None).await,
        Err(ThreadPlanError::InvalidInput)
    ));
    assert_eq!(store.read(&scope).await.unwrap(), Some(initial));
}

#[tokio::test]
async fn concurrent_replacements_have_distinct_monotonic_revisions() {
    let store = Arc::new(FilesystemThreadPlanStore::new(filesystem()));
    let scope = scope();
    let (first, second) = tokio::join!(
        store.update(&scope, write(ThreadPlanStatus::Pending), None),
        store.update(&scope, write(ThreadPlanStatus::Completed), None)
    );
    let mut revisions = vec![first.unwrap().revision, second.unwrap().revision];
    revisions.sort_unstable();
    assert_eq!(revisions, [1, 2]);
    assert_eq!(store.read(&scope).await.unwrap().unwrap().revision, 2);
}

#[tokio::test]
async fn read_permission_failure_is_an_error_not_empty_and_cannot_write() {
    let store = FilesystemThreadPlanStore::new(Arc::new(ScopedFilesystem::new(
        Arc::new(InMemoryBackend::new()),
        |_scope: &ResourceScope| Ok(MountView::default()),
    )));
    assert!(matches!(
        store.read(&scope()).await,
        Err(ThreadPlanError::StorageUnavailable)
    ));
    assert!(matches!(
        store
            .update(&scope(), write(ThreadPlanStatus::Pending), None)
            .await,
        Err(ThreadPlanError::StorageUnavailable)
    ));
}

#[tokio::test]
async fn corrupt_record_is_not_empty_and_is_never_overwritten() {
    let backend = Arc::new(InMemoryBackend::new());
    let fs = Arc::new(ScopedFilesystem::new(
        backend.clone(),
        |_scope: &ResourceScope| {
            MountView::new(vec![MountGrant::new(
                MountAlias::new("/threads")?,
                VirtualPath::new("/threads")?,
                MountPermissions::read_write_list_delete(),
            )])
        },
    ));
    let store = FilesystemThreadPlanStore::new(fs);
    store
        .update(&scope(), write(ThreadPlanStatus::Pending), None)
        .await
        .unwrap();
    let entries = backend
        .list_dir(&VirtualPath::new("/threads/plans").unwrap())
        .await
        .unwrap();
    let path = &entries[0].path;
    backend
        .put(
            path,
            Entry::bytes(b"broken record".to_vec()),
            CasExpectation::Any,
        )
        .await
        .unwrap();
    assert!(matches!(
        store.read(&scope()).await,
        Err(ThreadPlanError::InvalidRecord)
    ));
    assert!(matches!(
        store
            .update(&scope(), write(ThreadPlanStatus::Completed), None)
            .await,
        Err(ThreadPlanError::InvalidRecord)
    ));
    assert_eq!(
        backend.get(path).await.unwrap().unwrap().entry.body,
        b"broken record"
    );
}
