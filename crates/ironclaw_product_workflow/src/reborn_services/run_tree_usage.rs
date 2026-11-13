//! Add as reborn_services/run_tree_usage.rs; called by trusted normal API adapter.
//! No HTTP route, admin authority, session store, or dispatch introduced.
use super::*;
use async_trait::async_trait;
use ironclaw_turns::{
    RunTreeUsageAccess, RunTreeUsagePage, RunTreeUsagePrefixSource, RunTreeUsageSession, TurnError,
    TurnScope,
};
/// Normal caller-bound handle. Fields private; no Deserialize/DTO constructor.
pub struct OwnerRunTreeUsageSession {
    inner: RunTreeUsageSession,
    original_actor: ironclaw_host_api::UserId,
}
impl OwnerRunTreeUsageSession {
    pub fn root_scope(&self) -> &TurnScope {
        self.inner.root_scope()
    }
    pub fn root_run_id(&self) -> ironclaw_turns::TurnRunId {
        self.inner.root_run_id()
    }
    pub fn log_generation(&self) -> &str {
        self.inner.log_generation()
    }
    pub fn original_actor(&self) -> &ironclaw_host_api::UserId {
        &self.original_actor
    }
}

fn require_original_actor(
    original: &ironclaw_host_api::UserId,
    current: &ironclaw_host_api::UserId,
) -> Result<(), TurnError> {
    if original != current {
        return Err(TurnError::ScopeNotFound);
    }
    Ok(())
}

struct CurrentOwnerAccess<'a> {
    services: &'a RebornServices,
    caller: WebUiAuthenticatedCaller,
}
#[async_trait]
impl RunTreeUsageAccess for CurrentOwnerAccess<'_> {
    async fn authorize_exact_scope(&self, actual: &TurnScope) -> Result<(), TurnError> {
        let scope = self.caller.turn_scope(actual.thread_id.clone());
        let actor = self.caller.actor();
        let access = self
            .services
            .resolve_thread_access_for_caller(self.caller.clone(), scope, &actor)
            .await
            .map_err(|_| TurnError::ScopeNotFound)?;
        if access.scope != *actual {
            return Err(TurnError::ScopeNotFound);
        }
        Ok(())
    }
}
impl RebornServices {
    /// The injected reader must be the SAME authority as the normal runtime.
    /// Factory must reject wiring another checkout/store; no default fake reader.
    pub fn with_run_tree_usage_source(mut self, source: Arc<dyn RunTreeUsagePrefixSource>) -> Self {
        self.run_tree_usage_source = Some(source);
        self
    }
    pub async fn open_run_tree_usage(
        &self,
        caller: WebUiAuthenticatedCaller,
        request: RebornGetRunStateRequest,
    ) -> Result<OwnerRunTreeUsageSession, RebornServicesError> {
        let thread_id = parse_thread_id_field("thread_id", request.thread_id)?;
        let root = parse_run_id_field("run_id", request.run_id)?;
        let access = self
            .resolve_thread_access_for_caller(
                caller.clone(),
                caller.turn_scope(thread_id),
                &caller.actor(),
            )
            .await?;
        let source = self
            .run_tree_usage_source
            .as_ref()
            .ok_or_else(|| RebornServicesError::service_unavailable(false))?;
        let session = source
            .open_usage_tree(access.scope.clone(), root)
            .await
            .map_err(map_turn_error)?;
        // Return no rows until read_run_tree_usage_page reauthorizes all members.
        if session.root_scope() != &access.scope || session.root_run_id() != root {
            return Err(map_turn_error(TurnError::ScopeNotFound));
        }
        Ok(OwnerRunTreeUsageSession {
            inner: session,
            original_actor: caller.actor().user_id.clone(),
        })
    }
    // Reuse the NORMAL ownership/project resolver on EVERY page. A caller with
    // no default project must resolve the actual authorized stored scope again,
    // rather than recreating a projectless scope or trusting a query parameter.
    async fn current_usage_root_scope(
        &self,
        caller: WebUiAuthenticatedCaller,
        thread_id: ironclaw_host_api::ThreadId,
    ) -> Result<TurnScope, RebornServicesError> {
        let access = self
            .resolve_thread_access_for_caller(
                caller.clone(),
                caller.turn_scope(thread_id),
                &caller.actor(),
            )
            .await?;
        Ok(access.scope)
    }
    pub async fn read_run_tree_usage_page(
        &self,
        caller: WebUiAuthenticatedCaller,
        session: &mut OwnerRunTreeUsageSession,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<RunTreeUsagePage, RebornServicesError> {
        require_original_actor(&session.original_actor, &caller.actor().user_id)
            .map_err(map_turn_error)?;
        let session = &mut session.inner;
        let source = self
            .run_tree_usage_source
            .as_ref()
            .ok_or_else(|| RebornServicesError::service_unavailable(false))?;
        if source
            .current_usage_log_generation()
            .await
            .map_err(map_turn_error)?
            != session.log_generation()
        {
            return Err(map_turn_error(TurnError::ScopeNotFound));
        }
        let root_scope = self
            .current_usage_root_scope(caller.clone(), session.root_scope().thread_id.clone())
            .await?;
        let access = CurrentOwnerAccess {
            services: self,
            caller,
        };
        let page = session
            .read_page(&access, &root_scope, cursor, limit)
            .await
            .map_err(map_turn_error)?;
        if source
            .current_usage_log_generation()
            .await
            .map_err(map_turn_error)?
            != session.log_generation()
        {
            return Err(map_turn_error(TurnError::ScopeNotFound));
        }
        Ok(page)
    }
}

#[cfg(test)]
mod caller_tests {
    use super::*;
    #[test]
    fn same_scope_different_actual_actor_cannot_reuse_session() {
        let a = ironclaw_host_api::UserId::new("owner-a").unwrap();
        let b = ironclaw_host_api::UserId::new("owner-b").unwrap();
        assert!(require_original_actor(&a, &a).is_ok());
        assert!(require_original_actor(&a, &b).is_err());
    }
}

#[cfg(test)]
#[path = "run_tree_usage/fallback_tests.rs"]
mod fallback_tests;
