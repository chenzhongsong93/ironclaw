//! 使用真实 FilesystemProjectRepository 的产品层 standing owner 合同。
//! 不用 FakeRepo，不声称这些无网络测试证明旧Gateway或PG已修复。
use super::*;
use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
use ironclaw_host_api::{AgentId, MountAlias, MountGrant, MountPermissions, MountView, VirtualPath};
use ironclaw_projects::FilesystemProjectRepository;

type RealRepo = FilesystemProjectRepository<InMemoryBackend>;

fn caller(user: &str) -> ProjectCaller {
    ProjectCaller {
        tenant_id: TenantId::new("standing-owner-tenant").unwrap(),
        user_id: UserId::new(user).unwrap(),
    }
}

fn service() -> (RebornProjectService, Arc<RealRepo>) {
    let root = Arc::new(InMemoryBackend::default());
    let view = MountView::new(vec![MountGrant::new(
        MountAlias::new("/tenant-shared").unwrap(),
        VirtualPath::new("/tenants/host/shared").unwrap(),
        MountPermissions::read_write_list_delete(),
    )]).unwrap();
    let fs = Arc::new(ScopedFilesystem::with_fixed_view(root, view));
    let repo = Arc::new(FilesystemProjectRepository::new(
        fs, UserId::new("project-control").unwrap(), AgentId::new("project-control").unwrap(),
    ));
    (RebornProjectService::new(repo.clone()), repo)
}

async fn create(service: &RebornProjectService, id: &str) -> RebornProjectInfo {
    service.create_project(caller("owner"), RebornCreateProjectRequest {
        project_id: Some(id.into()), name: "真实repo standing owner合同".into(),
        description: String::new(), icon: None, color: None,
        metadata: Some(serde_json::json!({"source":"public-contract-fixture"})),
    }).await.unwrap().project
}

async fn members(service: &RebornProjectService, id: &str) -> Vec<RebornProjectMemberInfo> {
    service.list_members(caller("owner"), RebornListMembersRequest {project_id:id.into()})
        .await.unwrap().members
}

fn assert_owner(rows: &[RebornProjectMemberInfo], created: &RebornProjectInfo) {
    let owners: Vec<_> = rows.iter().filter(|row| row.user_id=="owner").collect();
    assert_eq!(owners.len(),1,"产品成员视图必须恰含一个真实standing owner");
    let owner=owners[0];
    assert_eq!(owner.role,RebornProjectRole::Owner);
    assert_eq!(owner.status,RebornProjectMemberStatus::Active);
    assert_eq!(owner.granted_by,"owner");
    assert_eq!(owner.created_at,created.created_at);
    assert_eq!(owner.updated_at,created.updated_at);
}

#[tokio::test]
async fn real_repo_product_create_reads_active_standing_owner_without_explicit_grant() {
    let (service,repo)=service();let project=create(&service,"standing-owner-new").await;
    assert_eq!(project.role,RebornProjectRole::Owner);
    let fetched=service.get_project(caller("owner"),RebornGetProjectRequest {project_id:project.project_id.clone()}).await.unwrap();
    assert_eq!(fetched.project.role,RebornProjectRole::Owner);
    // 原repository合同保留：没有把派生owner伪写成显式grant。
    assert!(repo.list_members(&caller("owner").tenant_id,&ProjectId::new(&project.project_id).unwrap()).await.unwrap().is_empty());
    let actual=members(&service,&project.project_id).await;assert_eq!(actual.len(),1);assert_owner(&actual,&project);
}

#[tokio::test]
async fn real_repo_product_add_remove_other_member_preserves_standing_owner() {
    let (service,repo)=service();let project=create(&service,"standing-owner-members").await;
    service.add_member(caller("owner"),RebornAddMemberRequest {project_id:project.project_id.clone(),user_id:"other".into(),role:RebornProjectRole::Editor}).await.unwrap();
    let actual=members(&service,&project.project_id).await;assert_eq!(actual.len(),2);assert_owner(&actual,&project);
    assert!(actual.iter().any(|row|row.user_id=="other"&&row.role==RebornProjectRole::Editor&&row.status==RebornProjectMemberStatus::Active));
    service.remove_member(caller("owner"),RebornRemoveMemberRequest {project_id:project.project_id.clone(),user_id:"other".into()}).await.unwrap();
    let actual=members(&service,&project.project_id).await;assert_eq!(actual.len(),1);assert_owner(&actual,&project);
    let explicit=repo.list_members(&caller("owner").tenant_id,&ProjectId::new(&project.project_id).unwrap()).await.unwrap();
    assert!(explicit.iter().all(|row|row.user_id.as_str()!="owner"));
}

#[tokio::test]
async fn real_repo_product_explicit_owner_anomaly_cannot_duplicate_or_downgrade_owner() {
    let (service,repo)=service();let project=create(&service,"standing-owner-anomaly").await;
    let tenant=caller("owner").tenant_id;let project_id=ProjectId::new(&project.project_id).unwrap();
    for (role,status) in [(ProjectRole::Viewer,ProjectMemberStatus::Revoked),(ProjectRole::Editor,ProjectMemberStatus::Active)] {
        repo.upsert_member(ProjectMemberRecord {tenant_id:tenant.clone(),project_id:project_id.clone(),user_id:UserId::new("owner").unwrap(),role,status,
            granted_by:UserId::new("other").unwrap(),created_at:chrono::Utc::now(),updated_at:chrono::Utc::now()}).await.unwrap();
        let actual=members(&service,&project.project_id).await;assert_eq!(actual.len(),1);assert_owner(&actual,&project);
        assert_eq!(repo.resolve_access(&tenant,&project_id,&UserId::new("owner").unwrap()).await.unwrap(),Some(ProjectRole::Owner));
        // 产品派生视图不改写原异常显式记录，保留审计可见性。
        let explicit=repo.list_members(&tenant,&project_id).await.unwrap();assert_eq!(explicit.len(),1);assert_eq!(explicit[0].role,role);assert_eq!(explicit[0].status,status);
    }
}

#[tokio::test]
async fn real_repo_product_owner_projection_keeps_unknown_foreign_and_tenant_denials() {
    let (service,_repo)=service();let project=create(&service,"standing-owner-isolation").await;
    for (who,id) in [(caller("stranger"),project.project_id.clone()),(caller("owner"),"unknown-project".into()),
        (ProjectCaller {tenant_id:TenantId::new("foreign-tenant").unwrap(),user_id:UserId::new("owner").unwrap()},project.project_id.clone())] {
        let error=service.list_members(who,RebornListMembersRequest {project_id:id}).await.unwrap_err();
        assert_eq!(error,ProjectServiceError::NotFound);
    }
}
