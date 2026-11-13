//! InProc normal resolver boundary, NOT PG/HTTP or real usage qualification.
use super::*;
use ironclaw_host_api::{AgentId, ProjectId, TenantId, UserId};
use ironclaw_threads::InMemorySessionThreadService;
use ironclaw_turns::{DefaultTurnCoordinator, test_support::in_memory_turn_state_store};

// Copied unchanged from the existing facade contract fixture. Only the project
// catalog is a trait fixture; create/read thread and the resolver are production.
#[derive(Debug)]
struct AuthorizingProjectService {
    allowed_project_id: String,
}

#[async_trait]
impl ProjectService for AuthorizingProjectService {
    async fn list_projects(
        &self,
        _caller: ProjectCaller,
        _request: RebornListProjectsRequest,
    ) -> Result<RebornListProjectsResponse, ProjectServiceError> {
        Ok(RebornListProjectsResponse {
            projects: vec![RebornProjectInfo {
                project_id: self.allowed_project_id.clone(),
                name: "Authorized".to_string(),
                description: String::new(),
                icon: None,
                color: None,
                metadata: serde_json::json!({}),
                state: RebornProjectState::Active,
                role: RebornProjectRole::Owner,
                created_at: "1970-01-01T00:00:00Z".parse().expect("created at"),
                updated_at: "1970-01-01T00:00:00Z".parse().expect("updated at"),
            }],
        })
    }

    async fn create_project(
        &self,
        _caller: ProjectCaller,
        _request: RebornCreateProjectRequest,
    ) -> Result<RebornProjectResponse, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn get_project(
        &self,
        _caller: ProjectCaller,
        request: RebornGetProjectRequest,
    ) -> Result<RebornProjectResponse, ProjectServiceError> {
        if request.project_id == self.allowed_project_id {
            Ok(RebornProjectResponse {
                project: RebornProjectInfo {
                    project_id: self.allowed_project_id.clone(),
                    name: "Authorized".to_string(),
                    description: String::new(),
                    icon: None,
                    color: None,
                    metadata: serde_json::json!({}),
                    state: RebornProjectState::Active,
                    role: RebornProjectRole::Owner,
                    created_at: "1970-01-01T00:00:00Z".parse().expect("created at"),
                    updated_at: "1970-01-01T00:00:00Z".parse().expect("updated at"),
                },
            })
        } else {
            // Mirrors the real service: no access (or unknown) collapses to NotFound.
            Err(ProjectServiceError::NotFound)
        }
    }

    async fn update_project(
        &self,
        _caller: ProjectCaller,
        _request: RebornUpdateProjectRequest,
    ) -> Result<RebornProjectResponse, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn delete_project(
        &self,
        _caller: ProjectCaller,
        _request: RebornDeleteProjectRequest,
    ) -> Result<(), ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn list_members(
        &self,
        _caller: ProjectCaller,
        _request: RebornListMembersRequest,
    ) -> Result<RebornListMembersResponse, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn add_member(
        &self,
        _caller: ProjectCaller,
        _request: RebornAddMemberRequest,
    ) -> Result<RebornProjectMemberInfo, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn update_member_role(
        &self,
        _caller: ProjectCaller,
        _request: RebornUpdateMemberRoleRequest,
    ) -> Result<RebornProjectMemberInfo, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn remove_member(
        &self,
        _caller: ProjectCaller,
        _request: RebornRemoveMemberRequest,
    ) -> Result<(), ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }
}
fn actual_caller(user: &str, project: Option<&str>) -> WebUiAuthenticatedCaller {
    WebUiAuthenticatedCaller::new(
        TenantId::new("tenant-alpha").unwrap(),
        UserId::new(user).unwrap(),
        Some(AgentId::new("agent-alpha").unwrap()),
        project.map(|p| ProjectId::new(p).unwrap()),
    )
}
#[tokio::test]
async fn page_scope_reuses_normal_authorized_project_fallback_and_rejects_foreign_scope() {
    let services = RebornServices::new(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(DefaultTurnCoordinator::new(Arc::new(
            in_memory_turn_state_store(),
        ))),
    )
    .with_project_service(Arc::new(AuthorizingProjectService {
        allowed_project_id: "project-scoped".into(),
    }));
    services.create_thread(actual_caller("user-alpha", None),
        serde_json::from_value(serde_json::json!({
            "client_action_id":"usage-fallback-create", "requested_thread_id":"usage-fallback-thread",
            "project_id":"project-scoped"
        })).unwrap()).await.unwrap();
    let thread = ironclaw_host_api::ThreadId::new("usage-fallback-thread").unwrap();
    let resolved = services
        .current_usage_root_scope(actual_caller("user-alpha", None), thread.clone())
        .await
        .unwrap();
    assert_eq!(
        resolved,
        actual_caller("user-alpha", Some("project-scoped")).turn_scope(thread.clone())
    );
    // The old caller.turn_scope(None) value is genuinely different; the lower
    // session exact equality must remain unchanged rather than being bypassed.
    assert_ne!(
        resolved,
        actual_caller("user-alpha", None).turn_scope(thread.clone())
    );
    assert!(
        services
            .current_usage_root_scope(
                actual_caller("user-alpha", Some("wrong-project")),
                thread.clone()
            )
            .await
            .is_err()
    );
    assert!(
        services
            .current_usage_root_scope(actual_caller("foreign-user", None), thread)
            .await
            .is_err()
    );
}
