//! Canonical per-caller thread-scope resolution.
//!
//! Multi-user WebChat pins each run to its authenticated caller, and the
//! loop host writes that run's thread under `owners/<caller>`. Every
//! subsequent read/write for the run must resolve the SAME owner — the
//! loop host's thread ports ([`crate::loop_driver_host`]), the loop-exit
//! completion-evidence read ([`crate::loop_exit_applier`]), and any
//! composition-side durable thread append that is keyed by a
//! [`LoopRunContext`](ironclaw_turns::run_profile::LoopRunContext).
//!
//! [`ThreadScopeResolver::resolve`] is the single definition of that
//! owner-rewrite rule. Both subsystems call it, so the rule cannot drift
//! between them — a second hand-rolled copy silently regressing
//! multi-user isolation is exactly the maintainability hazard this
//! removes.

use ironclaw_threads::ThreadScope;
use ironclaw_turns::{TurnActor, TurnScope};

/// Canonical owner-scoping rule for per-caller thread isolation.
pub struct ThreadScopeResolver;

impl ThreadScopeResolver {
    /// Re-point `base`'s project and `owner_user_id` at the claimed turn so
    /// project threads and each caller's `owners/<user>` subtree are read and
    /// written consistently for the run.
    ///
    /// Only rewrites when the base scope is owner-scoped: an owner-less
    /// base (no declared owner) or an actor-less run is returned
    /// unchanged, so single-operator and system flows are untouched.
    pub fn resolve(base: &ThreadScope, actor: Option<&TurnActor>) -> ThreadScope {
        let mut scope = base.clone();
        if scope.owner_user_id.is_some()
            && let Some(actor) = actor
        {
            scope.owner_user_id = Some(actor.user_id.clone());
        }
        scope
    }

    pub fn resolve_for_turn(
        base: &ThreadScope,
        turn_scope: &TurnScope,
        actor: Option<&TurnActor>,
    ) -> ThreadScope {
        // A runtime is composed once for an identity, while WebUI threads may
        // be created inside different projects over that runtime's lifetime.
        // Keep the base tenant/agent as the composition boundary (the host
        // validator still rejects a mismatch), but bind the project axis to
        // the claimed turn so project-scoped transcript reads and writes use
        // the same durable subtree as the run. Leaving the composition-time
        // project here (usually `None`) makes every project thread fail closed
        // during host construction before a model request is emitted.
        let mut scope = base.clone();
        scope.project_id = turn_scope.project_id.clone();
        if turn_scope.has_explicit_thread_owner() {
            scope.owner_user_id = turn_scope.explicit_owner_user_id().cloned();
            return scope;
        }
        Self::resolve(&scope, actor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::{AgentId, ProjectId, TenantId, UserId};

    fn scope(owner: Option<&str>) -> ThreadScope {
        ThreadScope {
            tenant_id: TenantId::new("tenant").expect("tenant"),
            agent_id: AgentId::new("agent").expect("agent"),
            project_id: None,
            owner_user_id: owner.map(|o| UserId::new(o).expect("user")),
            mission_id: None,
        }
    }

    fn actor(user: &str) -> TurnActor {
        TurnActor::new(UserId::new(user).expect("user"))
    }

    #[test]
    fn rewrites_owner_to_run_actor_when_base_is_owner_scoped() {
        let base = scope(Some("runtime-owner"));
        let a = ThreadScopeResolver::resolve(&base, Some(&actor("alice")));
        let b = ThreadScopeResolver::resolve(&base, Some(&actor("bob")));
        assert_eq!(a.owner_user_id.as_ref().map(|u| u.as_str()), Some("alice"));
        assert_eq!(b.owner_user_id.as_ref().map(|u| u.as_str()), Some("bob"));
    }

    #[test]
    fn leaves_owner_unchanged_when_run_has_no_actor() {
        let base = scope(Some("runtime-owner"));
        let resolved = ThreadScopeResolver::resolve(&base, None);
        assert_eq!(
            resolved.owner_user_id.as_ref().map(|u| u.as_str()),
            Some("runtime-owner"),
        );
    }

    #[test]
    fn leaves_owner_less_base_unchanged_even_with_an_actor() {
        let base = scope(None);
        let resolved = ThreadScopeResolver::resolve(&base, Some(&actor("alice")));
        assert!(
            resolved.owner_user_id.is_none(),
            "an owner-agnostic base must stay system/shared-scoped"
        );
    }

    #[test]
    fn explicit_turn_owner_overrides_actor_rewrite() {
        let base = scope(Some("runtime-owner"));
        let turn_scope = TurnScope::new_with_owner(
            base.tenant_id.clone(),
            Some(base.agent_id.clone()),
            base.project_id.clone(),
            ironclaw_host_api::ThreadId::new("thread").unwrap(),
            None,
        );

        let resolved =
            ThreadScopeResolver::resolve_for_turn(&base, &turn_scope, Some(&actor("alice")));

        assert_eq!(resolved.owner_user_id, None);
    }

    #[test]
    fn project_axis_follows_the_claimed_turn_scope() {
        let base = scope(Some("runtime-owner"));
        let turn_scope = TurnScope::new_with_owner(
            base.tenant_id.clone(),
            Some(base.agent_id.clone()),
            Some(ProjectId::new("project-turn").unwrap()),
            ironclaw_host_api::ThreadId::new("thread").unwrap(),
            Some(UserId::new("alice").unwrap()),
        );

        let resolved = ThreadScopeResolver::resolve_for_turn(&base, &turn_scope, None);

        assert_eq!(
            resolved.project_id.as_ref().map(|id| id.as_str()),
            Some("project-turn")
        );
        assert_eq!(
            resolved.owner_user_id.as_ref().map(|id| id.as_str()),
            Some("alice")
        );
    }

    #[test]
    fn project_axis_is_cleared_for_a_projectless_turn() {
        let mut base = scope(Some("runtime-owner"));
        base.project_id = Some(ProjectId::new("stale-project").unwrap());
        let turn_scope = TurnScope::new_with_owner(
            base.tenant_id.clone(),
            Some(base.agent_id.clone()),
            None,
            ironclaw_host_api::ThreadId::new("thread").unwrap(),
            Some(UserId::new("alice").unwrap()),
        );

        let resolved = ThreadScopeResolver::resolve_for_turn(&base, &turn_scope, None);

        assert_eq!(resolved.project_id, None);
    }
}
