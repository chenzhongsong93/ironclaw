//! Tianquan builtin `before_capability` guard.
//!
//! Cross-repo (sister-repo) hard guard that curbs three LLM anti-patterns
//! observed when Tianquan drives ironclaw's builtin tools (verify_l3 --rounds 3
//! reported 0/3):
//!
//! 1. **result_read polling after spawn.** `builtin.spawn_subagent` is a
//!    *blocking* capability: the loop resumes only when the subagent run
//!    completes. An LLM that spawns and then immediately calls
//!    `builtin.result_read` to "poll" the subagent result is abusing the tool
//!    surface — the result cannot be ready, the call wastes a capability
//!    round-trip, and it competes with the blocking resume. While a spawn is
//!    pending (subagent has not resumed the loop), `result_read` is denied.
//! 2. **duplicate spawn within a turn.** An LLM that has already spawned a
//!    subagent and spawns *again* before the first resumes is double-booking
//!    the turn; the first spawn is blocking and a second cannot meaningfully
//!    proceed. The second (and later) spawn is denied.
//! 3. **builtin.shell over-use.** `builtin.shell` is an escape hatch; an LLM
//!    leaning on it to bypass MCP tooling is an anti-pattern. A per-turn
//!    rate limit caps it.
//!
//! # Why ironclaw-side (not Tianquan-side)
//!
//! The builtin tools (`builtin.spawn_subagent` / `builtin.result_read` /
//! `builtin.shell`) are ironclaw-internal capabilities. Tianquan has no
//! dispatch authority over them — it cannot intercept their invocation — so a
//! prompt-layer (SKILL) constraint is advisory at best. The `before_capability`
//! hook is the only point where a hard deny can be enforced for these
//! builtins, hence this guard lives in the ironclaw fork.
//!
//! # Why coarse, argument-free rules
//!
//! [`BeforeCapabilityHookContext`] exposes only [`SanitizedArguments`], whose
//! sole extraction surface is [`SanitizedArguments::extract_numeric`] — it
//! cannot read the string-typed `result_ref` or shell command. The guard
//! therefore uses coarse, argument-free rules keyed on `capability_name` plus
//! cross-call state, not on argument content.
//!
//! # Spawn dispatch lifecycle
//!
//! The same guard instance is installed at both `BeforeCapability` and
//! `AfterCapability`. `BeforeCapability` only reserves an `Invoking` state; it
//! does not claim that a child exists. The bounded after-context then moves the
//! state to `Waiting` only when the returned resolution actually parks, or back
//! to `Idle` when the call returns without parking or errors. This prevents a
//! scope-recovery transient (or any other pre-dispatch failure) from creating a
//! ghost pending spawn that blocks an immediate retry.
//!
//! The lifecycle observer consumes only capability identity and the bounded
//! `Parked`/`Returned`/`Errored` classification; it never receives capability
//! input, output, gate detail, or host error text. `PENDING_SPAWN_WINDOW` remains
//! a bounded backstop for a lost after-observation or a hung child. A fresh

//! dispatcher is still minted per execution segment, so resume also resets the
//! state structurally.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use ironclaw_hooks::identity::{HookId, HookVersion};
use ironclaw_hooks::ordering::HookPhase;
use ironclaw_hooks::points::{
    BeforeCapabilityHookContext, ObservedCapabilityOutcome, ObservedKind, ObserverHookContext,
};
use ironclaw_hooks::registry::HookPointSpec;
use ironclaw_hooks::sink::{
    ObserverHook, ObserverSink, PrivilegedBeforeCapabilityHook, PrivilegedGateSink,
};

use ironclaw_turns::CapabilityActivityId;

use crate::error::RebornBuildError;

/// Canonical identity path for the Tianquan builtin guard. Stable across runs
/// so the binding is deterministic for checkpoint replay validation.
pub(crate) const TIANQUAN_GUARD_CANONICAL_PATH: &str =
    "ironclaw_reborn_composition::hooks::tianquan_guard::TianquanBuiltinGuard";
pub(super) const TIANQUAN_GUARD_OBSERVER_CANONICAL_PATH: &str =
    "ironclaw_reborn_composition::hooks::tianquan_guard::TianquanBuiltinGuardAfterCapability";

/// Window after an allowed spawn during which `result_read` is denied as a
/// "polling while spawn is blocking" anti-pattern. Sized at 300 s to cover
/// worst-case subagent prose generation (~200 s request-timeout/lease bound
/// plus margin); the previous 60 s window expired mid-generation and let the
/// polling anti-pattern resume. See the module docs for the full rationale.
pub(crate) const PENDING_SPAWN_WINDOW: Duration = Duration::from_secs(300);

/// Per-guard cap on `builtin.shell` invocations. The 4th call (and later) in a
/// guard lifetime is denied. Tuned to let an LLM do a small amount of shell
/// work (e.g. one quick check) while preventing shell-as-MCP-bypass.
pub(crate) const SHELL_LIMIT: u32 = 3;

/// Per-guard cap on **consecutive** invocations of the same capability (any
/// capability, including MCP tools such as `tianquan-graph.run_skill_verify`).
/// The Nth consecutive call (and later) is denied. Tuned to let an LLM do a
/// legitimate bounded retry (e.g. 2-3 verify passes) while cutting the
/// "same-tool infinite loop" anti-pattern that burns the context budget before
/// L8 (measured 2026-08-03: 30+ consecutive `run_skill_verify` calls in one
/// round). The counter resets as soon as a *different* capability is invoked,
/// so normal interleaved flows (verify -> check stage -> advance) are never
/// affected.
pub(crate) const REPEAT_CALL_LIMIT: u32 = 6;

/// Capability names this guard keys on. These are the ironclaw builtin
/// capability names; the guard is inert for any other capability (it `pass`es,
/// contributing nothing to the composed decision).
const CAPABILITY_SPAWN_SUBAGENT: &str = "builtin.spawn_subagent";
const CAPABILITY_RESULT_READ: &str = "builtin.result_read";
const CAPABILITY_SHELL: &str = "builtin.shell";

/// Deny reasons. These are `&'static str` because
/// [`PrivilegedGateSink::deny`] requires them to flow through the rustc literal
/// table (no dynamic `format!`-built strings can leak through the seam).
const REASON_RESULT_READ_DURING_PENDING_SPAWN: &str =
    "spawn 是 blocking,等 resume 不要 result_read 轮询";
const REASON_DUPLICATE_SPAWN: &str = "已 spawn,等 resume 不要重复 spawn";
const REASON_SHELL_RATE_LIMITED: &str = "builtin.shell 调用过多,用 MCP 工具而非 shell";
const REASON_REPEAT_CALL: &str = "同工具连续调用过多,勿空转重试:conforms 稳定则 advance_stage(to=approve) 进 committer,或 L7 完成切 loop:prose spawn_subagent 进 L8;真问题请报告而非重复调用";

#[derive(Debug, Clone, PartialEq, Eq)]
enum SpawnDispatchState {
    Idle,
    Invoking {
        activity_id: CapabilityActivityId,
        started_at: Instant,
    },
    Waiting {
        activity_id: CapabilityActivityId,
        started_at: Instant,
    },
}

/// Cross-call state for [`TianquanBuiltinGuard`]. Held behind an
/// `Arc<Mutex<...>>` so the guard (which is `Send + Sync + 'static`) can share
/// mutable counters across invocations within one dispatcher's lifetime.
#[derive(Debug, Clone)]
struct TianquanGuardState {
    /// Lifecycle of the one spawn dispatch allowed in this execution segment.
    spawn_dispatch: SpawnDispatchState,
    /// Cumulative count of `builtin.spawn_subagent` evaluations seen by this
    /// guard (both allowed and denied). Retained for observability/tests; the
    /// duplicate-spawn deny keys on the active dispatch state, not on this
    /// counter, so a legitimate re-spawn after the previous child's
    /// window expired is allowed.
    spawn_count: u32,
    /// Cumulative count of `builtin.shell` evaluations seen by this guard.
    /// Denied over-limit calls still increment so a storm of shell calls keeps
    /// the guard in "rate-limited" mode rather than flickering.
    shell_count: u32,
    /// Consecutive-call counters keyed by `capability_name`. Only *consecutive*
    /// calls of the same capability accumulate (the counter resets to 1 when a
    /// different capability is invoked). This implements the
    /// [`REPEAT_CALL_LIMIT`] deny for any capability — including MCP tools
    /// such as `tianquan-graph.run_skill_verify` (hook covers all capability
    /// kinds via `HookedLoopCapabilityPort`). Special-cased capabilities
    /// (spawn/result_read/shell) do not increment these counters, but every
    /// invocation still replaces `last_capability` so it interrupts an
    /// ordinary capability's consecutive-call sequence.
    repeat_calls: HashMap<String, u32>,
    /// The last capability invocation evaluated, including special
    /// capabilities and denials. Used to detect *consecutive* vs
    /// *interleaved* ordinary calls. `None` before the first evaluation.
    last_capability: Option<String>,
}

impl TianquanGuardState {
    fn new() -> Self {
        Self {
            spawn_dispatch: SpawnDispatchState::Idle,
            spawn_count: 0,
            shell_count: 0,
            repeat_calls: HashMap::new(),
            last_capability: None,
        }
    }

    /// Expire a stale in-flight/waiting spawn if the bounded backstop elapsed.
    fn expire_spawn_dispatch(&mut self, now: Instant) {
        let started_at = match &self.spawn_dispatch {
            SpawnDispatchState::Invoking { started_at, .. }
            | SpawnDispatchState::Waiting { started_at, .. } => Some(*started_at),
            SpawnDispatchState::Idle => None,
        };
        if started_at
            .is_some_and(|started_at| now.duration_since(started_at) >= PENDING_SPAWN_WINDOW)
        {
            self.spawn_dispatch = SpawnDispatchState::Idle;
        }
    }

    fn has_active_spawn(&self) -> bool {
        !matches!(self.spawn_dispatch, SpawnDispatchState::Idle)
    }

    fn observe_spawn_outcome(
        &mut self,
        activity_id: CapabilityActivityId,
        outcome: ObservedCapabilityOutcome,
    ) {
        let started_at = match &self.spawn_dispatch {
            SpawnDispatchState::Invoking {
                activity_id: pending_activity_id,
                started_at,
            } if *pending_activity_id == activity_id => *started_at,
            SpawnDispatchState::Idle
            | SpawnDispatchState::Waiting { .. }
            | SpawnDispatchState::Invoking { .. } => return,
        };
        self.spawn_dispatch = match outcome {
            ObservedCapabilityOutcome::Parked => SpawnDispatchState::Waiting {
                activity_id,
                started_at,
            },
            ObservedCapabilityOutcome::Returned | ObservedCapabilityOutcome::Errored => {
                SpawnDispatchState::Idle
            }
            _ => SpawnDispatchState::Idle,
        };
    }
}

/// A `before_capability` hook that curbs three Tianquan LLM anti-patterns on
/// ironclaw builtin tools. See the module docs.
///
/// Installed at the [`HookPhase::Policy`] phase as a `Builtin`-tier
/// (privileged) hook, so it may mint `deny` / `pass` and runs before
/// `Telemetry`-phase observers. The guard holds no authority to `allow` a
/// capability that another hook denies; composition keeps the most
/// restrictive decision.
#[derive(Debug, Clone)]
pub(crate) struct TianquanBuiltinGuard {
    state: Arc<Mutex<TianquanGuardState>>,
}

impl TianquanBuiltinGuard {
    /// Construct a guard with fresh counters.
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TianquanGuardState::new())),
        }
    }

    #[cfg(test)]
    fn spawn_dispatch_state(&self) -> SpawnDispatchState {
        self.state
            .lock()
            .expect("Tianquan guard state mutex poisoned")
            .spawn_dispatch
            .clone()
    }
}

impl Default for TianquanBuiltinGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ObserverHook for TianquanBuiltinGuard {
    async fn observe(&self, ctx: &ObserverHookContext, _sink: &mut dyn ObserverSink) {
        if ctx.observed_kind != ObservedKind::AfterCapability
            || ctx.capability_name.as_deref() != Some(CAPABILITY_SPAWN_SUBAGENT)
        {
            return;
        }
        let Some(outcome) = ctx.capability_outcome else {
            return;
        };
        let Some(activity_id) = ctx.capability_activity_id else {
            return;
        };
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        state.expire_spawn_dispatch(Instant::now());
        state.observe_spawn_outcome(activity_id, outcome);
    }
}

#[async_trait]
impl PrivilegedBeforeCapabilityHook for TianquanBuiltinGuard {
    async fn evaluate(&self, ctx: &BeforeCapabilityHookContext, sink: &mut dyn PrivilegedGateSink) {
        let now = Instant::now();
        let mut state = match self.state.lock() {
            Ok(guard) => guard,
            // Mutex poison means another invocation panicked mid-evaluation.
            // Fail closed: deny so the anti-pattern cannot proceed while state
            // is unreadable. This is conservative (may over-deny) but safe.
            Err(_) => {
                sink.deny("tianquan guard state unavailable");
                return;
            }
        };
        state.expire_spawn_dispatch(now);
        // Every invocation, including a special capability rejected by its
        // dedicated rule, breaks an ordinary capability's repeat sequence.
        // Preserve the prior value for the ordinary-capability calculation
        // below, then record this invocation before any early return.
        let repeats_previous_capability =
            state.last_capability.as_deref() == Some(ctx.capability_name.as_str());
        state.last_capability = Some(ctx.capability_name.clone());

        match ctx.capability_name.as_str() {
            CAPABILITY_SPAWN_SUBAGENT => {
                state.spawn_count = state.spawn_count.saturating_add(1);
                if state.has_active_spawn() {
                    // A previous spawn's blocking window is still active: a
                    // second spawn now is the duplicate-spawn anti-pattern.
                    // Once the window expires (child presumed resumed or hung
                    // past recovery), a fresh spawn is allowed again — this
                    // permits legitimate re-spawn (e.g. validator blocked the
                    // first child's prose, or the child hung).
                    drop(state);
                    sink.deny(REASON_DUPLICATE_SPAWN);
                    return;
                }
                // Reserve only the invocation. The matching AfterCapability
                // observation commits Waiting iff the returned resolution parks.
                let Some(activity_id) = ctx.activity_id else {
                    drop(state);
                    sink.deny("tianquan guard requires capability activity identity");
                    return;
                };
                state.spawn_dispatch = SpawnDispatchState::Invoking {
                    activity_id,
                    started_at: now,
                };
                drop(state);
                sink.pass();
            }
            CAPABILITY_RESULT_READ => {
                if state.has_active_spawn() {
                    // A spawn is still within its blocking window: result_read
                    // here is "polling a result that cannot be ready yet."
                    drop(state);
                    sink.deny(REASON_RESULT_READ_DURING_PENDING_SPAWN);
                    return;
                }
                drop(state);
                sink.pass();
            }
            CAPABILITY_SHELL => {
                state.shell_count = state.shell_count.saturating_add(1);
                if state.shell_count > SHELL_LIMIT {
                    drop(state);
                    sink.deny(REASON_SHELL_RATE_LIMITED);
                    return;
                }
                drop(state);
                sink.pass();
            }
            // Any other capability (including all MCP tools, e.g.
            // `tianquan-graph.run_skill_verify`): enforce the consecutive-call
            // limit. The counter only accumulates while the *same* capability
            // is invoked back-to-back; an interleaved call of any other
            // capability resets it to 1, so normal multi-step flows
            // (verify -> get_layer_stage -> advance_stage) never trip this.
            _ => {
                let cap_name = ctx.capability_name.clone();
                let count = if repeats_previous_capability {
                    state
                        .repeat_calls
                        .get(&cap_name)
                        .copied()
                        .unwrap_or(0)
                        .saturating_add(1)
                } else {
                    1
                };
                state.repeat_calls.insert(cap_name.clone(), count);
                if count >= REPEAT_CALL_LIMIT {
                    let detail = format!(
                        "{cap_name} 已连续调用 {count} 次未推进(上限 {REPEAT_CALL_LIMIT} 次)"
                    );
                    drop(state);
                    sink.record_audit_reason(detail);
                    sink.deny(REASON_REPEAT_CALL);
                    return;
                }
                drop(state);
                sink.pass();
            }
        }
    }

    /// This guard never reads capability arguments — its rules are keyed on
    /// `capability_name` and cross-call state only. Opting out of input lets
    /// the middleware skip eager argument resolution for invocations routed
    /// through this guard.
    fn needs_input(&self) -> bool {
        false
    }
}

/// Install the Tianquan builtin guard into `builder` as a first-party
/// `Builtin`-tier `before_capability` hook at the [`HookPhase::Policy`] phase.
///
/// This is the production first-party install step, replacing the empty
/// no-op catalog. It is a pure replayable function of its builder input: the
/// same call is proven to succeed against a scratch builder at composition
/// time and replayed per-run (see
/// `factory::build_hook_dispatcher_builder_factory_with`).
///
/// # Errors
///
/// Returns [`RebornBuildError::InvalidConfig`] if the dispatcher builder
/// rejects the binding (it should not, given a stable canonical id and a
/// Builtin-tier Policy phase, which `HookPhase::Policy::permits_trust` always
/// admits).
pub(crate) fn install_tianquan_guard(
    builder: ironclaw_hooks::dispatch::HookDispatcherBuilder,
) -> Result<ironclaw_hooks::dispatch::HookDispatcherBuilder, RebornBuildError> {
    let guard = TianquanBuiltinGuard::new();
    let before_hook_id = HookId::for_builtin(TIANQUAN_GUARD_CANONICAL_PATH, HookVersion::ONE);
    let observer_hook_id =
        HookId::for_builtin(TIANQUAN_GUARD_OBSERVER_CANONICAL_PATH, HookVersion::ONE);
    builder
        .install_builtin_before_capability(
            before_hook_id,
            HookPhase::Policy,
            Box::new(guard.clone()),
        )
        .and_then(|builder| {
            builder.install_builtin_observer(
                observer_hook_id,
                HookPhase::Telemetry,
                HookPointSpec::AfterCapability,
                Box::new(guard),
            )
        })
        .map_err(|error| RebornBuildError::InvalidConfig {
            reason: format!("failed to install tianquan builtin guard hook pair: {error}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_hooks::sink::PrivilegedGateSink;
    use ironclaw_host_api::TenantId;

    /// The outcome a [`CapturingSink`] recorded from a single `evaluate` call.
    /// Mirrors the dispatcher's `GateSinkState` (which is `pub(crate)` to
    /// `ironclaw_hooks` and therefore unreachable here) at the granularity the
    /// guard's tests need: did the guard pass, deny, or do something else?
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum CapturedOutcome {
        /// The guard called no sink method. The dispatcher treats this as a
        /// protocol violation (fail-closed); for the guard it should never
        /// happen — every branch calls `pass` or `deny`.
        Unset,
        Passed,
        Allowed,
        Denied,
        PausedApproval,
        PausedAuth,
    }

    /// Minimal `PrivilegedGateSink` impl that records the outcome. We cannot
    /// reuse `ironclaw_hooks::sink::RecordingGateSink` (it is `pub(crate)` to
    /// the hooks crate), so this local sink gives the tests the same
    /// observability without reaching across the crate boundary.
    struct CapturingSink {
        outcome: CapturedOutcome,
    }

    impl CapturingSink {
        fn new() -> Self {
            Self {
                outcome: CapturedOutcome::Unset,
            }
        }
    }

    impl ObserverSink for CapturingSink {
        fn note(
            &mut self,
            _category: ironclaw_hooks::kinds::observer::NoteCategory,
            _summary: &'static str,
        ) {
        }
    }

    impl PrivilegedGateSink for CapturingSink {
        fn allow(&mut self) {
            self.outcome = CapturedOutcome::Allowed;
        }
        fn deny(&mut self, _reason: &'static str) {
            self.outcome = CapturedOutcome::Denied;
        }
        fn pause_approval(&mut self, _reason: &'static str) {
            self.outcome = CapturedOutcome::PausedApproval;
        }
        fn pause_auth(&mut self, _reason: &'static str) {
            self.outcome = CapturedOutcome::PausedAuth;
        }
        fn pass(&mut self) {
            self.outcome = CapturedOutcome::Passed;
        }
        fn record_audit_reason(&mut self, _reason: String) {}
    }

    /// Build a context for `capability_name` with unresolved arguments. The
    /// guard does not read arguments, so an unresolved view is fine and matches
    /// the no-resolver middleware path.
    fn ctx_for(capability_name: &str) -> BeforeCapabilityHookContext {
        BeforeCapabilityHookContext::new_unresolved(
            TenantId::new("tianquan-test".to_string()).expect("valid tenant"),
            capability_name.to_string(),
            [0u8; 32],
        )
        .with_activity_id(CapabilityActivityId::new())
    }

    fn observe_capability_with_id(
        guard: &TianquanBuiltinGuard,
        capability_name: &str,
        activity_id: CapabilityActivityId,
        outcome: ObservedCapabilityOutcome,
    ) {
        let ctx = ObserverHookContext::after_capability(
            TenantId::new("tianquan-test".to_string()).expect("valid tenant"),
            capability_name.to_string(),
            activity_id,
            None,
            outcome,
        );
        let mut sink = CapturingSink::new();
        futures::executor::block_on(guard.observe(&ctx, &mut sink));
    }

    fn observe_capability(
        guard: &TianquanBuiltinGuard,
        capability_name: &str,
        outcome: ObservedCapabilityOutcome,
    ) {
        let activity_id = match guard.spawn_dispatch_state() {
            SpawnDispatchState::Invoking { activity_id, .. }
            | SpawnDispatchState::Waiting { activity_id, .. } => activity_id,
            SpawnDispatchState::Idle => CapabilityActivityId::new(),
        };
        observe_capability_with_id(guard, capability_name, activity_id, outcome);
    }

    /// Run the guard against `ctx` and return the captured outcome.
    fn evaluate(
        guard: &TianquanBuiltinGuard,
        ctx: &BeforeCapabilityHookContext,
    ) -> CapturedOutcome {
        let mut sink = CapturingSink::new();
        // The hook is async only because the trait is `async_trait`; the
        // implementation performs no `.await`. Block on it via a current-thread
        // future poll.
        futures::executor::block_on(guard.evaluate(ctx, &mut sink));
        sink.outcome
    }

    /// Helper: assert the outcome is `Denied`.
    fn assert_denied(outcome: CapturedOutcome) {
        assert_eq!(
            outcome,
            CapturedOutcome::Denied,
            "expected the guard to deny the capability"
        );
    }

    /// Helper: assert the outcome is `Passed` (the guard explicitly had no
    /// opinion, contributing nothing to the composed decision — the capability
    /// proceeds under other hooks' / the default Allow).
    fn assert_passed(outcome: CapturedOutcome) {
        assert_eq!(
            outcome,
            CapturedOutcome::Passed,
            "expected the guard to pass"
        );
    }

    #[test]
    fn result_read_during_pending_spawn_denied() {
        let guard = TianquanBuiltinGuard::new();
        // First spawn: allowed, reserves the invocation until AfterCapability.
        let spawn_outcome = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_passed(spawn_outcome);
        assert!(matches!(
            guard.spawn_dispatch_state(),
            SpawnDispatchState::Invoking { .. }
        ));
        // result_read while pending: denied.
        let read_outcome = evaluate(&guard, &ctx_for(CAPABILITY_RESULT_READ));
        assert_denied(read_outcome);
    }

    #[test]
    fn result_read_no_pending_spawn_allowed() {
        let guard = TianquanBuiltinGuard::new();
        // No spawn yet: pending_spawn is false, result_read passes.
        let read_outcome = evaluate(&guard, &ctx_for(CAPABILITY_RESULT_READ));
        assert_passed(read_outcome);
    }

    #[test]
    fn duplicate_spawn_denied() {
        let guard = TianquanBuiltinGuard::new();
        // First spawn: allowed.
        let first = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_passed(first);
        // Second spawn within the window: denied as a duplicate.
        let second = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_denied(second);
    }

    #[test]
    fn after_spawn_parked_commits_waiting_and_blocks_retry() {
        let guard = TianquanBuiltinGuard::new();
        let spawn = ctx_for(CAPABILITY_SPAWN_SUBAGENT);
        let activity_id = spawn.activity_id.expect("test spawn has activity id");
        assert_passed(evaluate(&guard, &spawn));
        observe_capability_with_id(
            &guard,
            CAPABILITY_SPAWN_SUBAGENT,
            activity_id,
            ObservedCapabilityOutcome::Parked,
        );

        assert!(matches!(
            guard.spawn_dispatch_state(),
            SpawnDispatchState::Waiting { .. }
        ));
        assert_denied(evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT)));
        assert_denied(evaluate(&guard, &ctx_for(CAPABILITY_RESULT_READ)));
    }

    #[test]
    fn rejected_concurrent_spawn_outcome_cannot_release_the_inflight_spawn() {
        let guard = TianquanBuiltinGuard::new();
        let first = ctx_for(CAPABILITY_SPAWN_SUBAGENT);
        let first_activity_id = first.activity_id.expect("test spawn has activity id");
        assert_passed(evaluate(&guard, &first));
        let second = ctx_for(CAPABILITY_SPAWN_SUBAGENT);
        let second_activity_id = second.activity_id.expect("test spawn has activity id");
        assert_denied(evaluate(&guard, &second));

        observe_capability_with_id(
            &guard,
            CAPABILITY_SPAWN_SUBAGENT,
            second_activity_id,
            ObservedCapabilityOutcome::Returned,
        );
        assert!(matches!(
            guard.spawn_dispatch_state(),
            SpawnDispatchState::Invoking { activity_id, .. } if activity_id == first_activity_id
        ));

        observe_capability_with_id(
            &guard,
            CAPABILITY_SPAWN_SUBAGENT,
            first_activity_id,
            ObservedCapabilityOutcome::Parked,
        );
        assert!(matches!(
            guard.spawn_dispatch_state(),
            SpawnDispatchState::Waiting { activity_id, .. } if activity_id == first_activity_id
        ));
    }

    #[test]
    fn after_spawn_returned_or_errored_releases_immediately_for_retry() {
        for outcome in [
            ObservedCapabilityOutcome::Returned,
            ObservedCapabilityOutcome::Errored,
        ] {
            let guard = TianquanBuiltinGuard::new();
            assert_passed(evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT)));
            observe_capability(&guard, CAPABILITY_SPAWN_SUBAGENT, outcome);

            assert_eq!(guard.spawn_dispatch_state(), SpawnDispatchState::Idle);
            assert_passed(evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT)));
        }
    }

    #[test]
    fn unrelated_or_late_after_observation_cannot_change_spawn_state() {
        let guard = TianquanBuiltinGuard::new();
        assert_passed(evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT)));

        observe_capability(
            &guard,
            CAPABILITY_RESULT_READ,
            ObservedCapabilityOutcome::Errored,
        );
        assert!(matches!(
            guard.spawn_dispatch_state(),
            SpawnDispatchState::Invoking { .. }
        ));

        observe_capability(
            &guard,
            CAPABILITY_SPAWN_SUBAGENT,
            ObservedCapabilityOutcome::Parked,
        );
        observe_capability(
            &guard,
            CAPABILITY_SPAWN_SUBAGENT,
            ObservedCapabilityOutcome::Returned,
        );
        assert!(matches!(
            guard.spawn_dispatch_state(),
            SpawnDispatchState::Waiting { .. }
        ));
    }

    #[test]
    fn respawn_allowed_after_window_expiry() {
        // The duplicate-spawn deny keys on the pending window, not a
        // cumulative count: once the window has elapsed (child presumed
        // resumed, or hung past recovery), a fresh spawn must be allowed so
        // legitimate re-spawn (validator blocked the first child's prose, or
        // the child hung) is not permanently blocked.
        //
        // `Instant` cannot be advanced, so set the state to the same Idle value
        // `expire_spawn_dispatch` would produce, then assert public behavior.
        let guard = TianquanBuiltinGuard::new();
        evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        let denied = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_denied(denied);

        {
            let mut state = guard
                .state
                .lock()
                .expect("Tianquan guard state mutex poisoned");
            state.spawn_dispatch = SpawnDispatchState::Idle;
        }

        let respawn = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_passed(respawn);
        assert!(matches!(
            guard.spawn_dispatch_state(),
            SpawnDispatchState::Invoking { .. }
        ));
    }

    #[test]
    fn pending_spawn_window_covers_subagent_generation() {
        // Pin the window sizing contract (handover 2026-07-31): minimax needs
        // ~115 s for a 3000-char chapter, the request timeout is 180 s and the
        // runner lease is 200 s, so the window must exceed the ~200 s
        // slow-but-healthy bound or result_read polling is re-allowed
        // mid-generation. It must also stay bounded so a hung child does not
        // block recovery re-spawn forever.
        assert!(
            PENDING_SPAWN_WINDOW >= Duration::from_secs(200),
            "window must cover worst-case subagent generation; saw {PENDING_SPAWN_WINDOW:?}"
        );
        assert!(
            PENDING_SPAWN_WINDOW <= Duration::from_secs(600),
            "window must stay bounded so a hung child can be re-spawned; saw {PENDING_SPAWN_WINDOW:?}"
        );
    }

    #[test]
    fn first_spawn_allowed_and_sets_pending() {
        let guard = TianquanBuiltinGuard::new();
        assert_eq!(guard.spawn_dispatch_state(), SpawnDispatchState::Idle);
        let outcome = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_passed(outcome);
        assert!(matches!(
            guard.spawn_dispatch_state(),
            SpawnDispatchState::Invoking { .. }
        ));
    }

    #[test]
    fn shell_rate_limited() {
        let guard = TianquanBuiltinGuard::new();
        // First SHELL_LIMIT (3) calls pass.
        for i in 0..SHELL_LIMIT {
            let outcome = evaluate(&guard, &ctx_for(CAPABILITY_SHELL));
            assert_eq!(
                outcome,
                CapturedOutcome::Passed,
                "shell call {} (0-indexed) should pass",
                i
            );
        }
        // 4th call: denied.
        let over = evaluate(&guard, &ctx_for(CAPABILITY_SHELL));
        assert_denied(over);
    }

    #[test]
    fn shell_under_limit_allowed() {
        let guard = TianquanBuiltinGuard::new();
        for _ in 0..SHELL_LIMIT {
            let outcome = evaluate(&guard, &ctx_for(CAPABILITY_SHELL));
            assert_passed(outcome);
        }
    }

    #[test]
    fn run_id_change_clears_state_via_time_window() {
        // The context carries no run_id (verified against
        // ironclaw_hooks::points::capability), so turn-boundary reset is
        // emulated by the time window. This test proves the state clears: a
        // spawn is allowed, then we simulate the window elapsing, and
        // result_read is no longer denied.
        //
        // Because `Instant` cannot be arbitrarily advanced, set the state to
        // the same Idle value `expire_spawn_dispatch` would produce, then assert
        // the public behavior. Once the window closes, the deny must lift.
        let guard = TianquanBuiltinGuard::new();
        evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert!(matches!(
            guard.spawn_dispatch_state(),
            SpawnDispatchState::Invoking { .. }
        ));

        {
            let mut state = guard
                .state
                .lock()
                .expect("Tianquan guard state mutex poisoned");
            state.spawn_dispatch = SpawnDispatchState::Idle;
        }

        let read_outcome = evaluate(&guard, &ctx_for(CAPABILITY_RESULT_READ));
        assert_passed(read_outcome);
    }

    #[test]
    fn other_capability_passes() {
        // A single call of a capability the guard does not otherwise key on
        // passes (count 1 < REPEAT_CALL_LIMIT) — the guard is inert for the
        // rest of the capability surface until the same tool is hammered
        // consecutively.
        let guard = TianquanBuiltinGuard::new();
        let outcome = evaluate(&guard, &ctx_for("builtin.some_other_tool"));
        assert_passed(outcome);
    }

    #[test]
    fn repeat_call_denied_after_limit() {
        // The L7 VERIFY infinite-loop anti-pattern (measured 2026-08-03: 30+
        // consecutive `run_skill_verify` calls in one round burning the context
        // budget before L8). REPEAT_CALL_LIMIT - 1 consecutive calls pass; the
        // LIMIT-th consecutive call is denied.
        let guard = TianquanBuiltinGuard::new();
        for i in 0..REPEAT_CALL_LIMIT - 1 {
            let outcome = evaluate(&guard, &ctx_for("tianquan-graph.run_skill_verify"));
            assert_eq!(
                outcome,
                CapturedOutcome::Passed,
                "consecutive call {} (0-indexed) should pass",
                i
            );
        }
        let over = evaluate(&guard, &ctx_for("tianquan-graph.run_skill_verify"));
        assert_denied(over);
    }

    #[test]
    fn repeat_call_resets_on_interleaved_capability() {
        // Interleaving any *different* capability resets the consecutive
        // counter: verify -> get_layer_stage -> verify... is a normal flow and
        // must never be denied even after many total verify calls.
        let guard = TianquanBuiltinGuard::new();
        for _ in 0..REPEAT_CALL_LIMIT + 2 {
            let outcome = evaluate(&guard, &ctx_for("tianquan-graph.run_skill_verify"));
            assert_passed(outcome);
            let interleave = evaluate(&guard, &ctx_for("tianquan-graph.get_layer_stage"));
            assert_passed(interleave);
        }
    }

    #[test]
    fn repeat_call_is_interrupted_by_shell_invocation() {
        // A special capability is still an invocation boundary for ordinary
        // capability repetition. The sixth ordinary invocation would normally
        // be denied, but shell breaks that sequence without changing its own
        // rate-limit behavior.
        let guard = TianquanBuiltinGuard::new();
        for _ in 0..REPEAT_CALL_LIMIT - 1 {
            assert_passed(evaluate(
                &guard,
                &ctx_for("tianquan-graph.run_skill_verify"),
            ));
        }

        assert_passed(evaluate(&guard, &ctx_for(CAPABILITY_SHELL)));
        assert_passed(evaluate(
            &guard,
            &ctx_for("tianquan-graph.run_skill_verify"),
        ));
    }

    #[test]
    fn repeat_call_is_interrupted_by_spawn_invocation() {
        let guard = TianquanBuiltinGuard::new();
        for _ in 0..REPEAT_CALL_LIMIT - 1 {
            assert_passed(evaluate(
                &guard,
                &ctx_for("tianquan-graph.run_skill_verify"),
            ));
        }

        assert_passed(evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT)));
        assert_passed(evaluate(
            &guard,
            &ctx_for("tianquan-graph.run_skill_verify"),
        ));
    }

    #[test]
    fn repeat_call_is_interrupted_by_denied_result_read() {
        let guard = TianquanBuiltinGuard::new();
        for _ in 0..REPEAT_CALL_LIMIT - 1 {
            assert_passed(evaluate(
                &guard,
                &ctx_for("tianquan-graph.run_skill_verify"),
            ));
        }

        assert_passed(evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT)));
        assert_denied(evaluate(&guard, &ctx_for(CAPABILITY_RESULT_READ)));
        assert_passed(evaluate(
            &guard,
            &ctx_for("tianquan-graph.run_skill_verify"),
        ));
    }

    #[test]
    fn repeat_call_exempts_special_capabilities() {
        // spawn / result_read / shell do not feed the consecutive-call counter
        // (they are handled by their own dedicated rules). A storm of shell
        // calls followed by a single other-tool call must not be treated as a
        // 10-consecutive repeat of that other tool.
        let guard = TianquanBuiltinGuard::new();
        for _ in 0..SHELL_LIMIT {
            assert_passed(evaluate(&guard, &ctx_for(CAPABILITY_SHELL)));
        }
        assert_denied(evaluate(&guard, &ctx_for(CAPABILITY_SHELL)));
        // A fresh tool after the shell storm starts its counter at 1.
        let outcome = evaluate(&guard, &ctx_for("tianquan-graph.get_layer_stage"));
        assert_passed(outcome);
    }

    #[tokio::test]
    async fn middleware_releases_transient_spawn_for_immediate_retry_then_commits_parked_spawn() {
        use std::collections::VecDeque;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use ironclaw_hooks::dispatch::HookDispatcherBuilder;
        use ironclaw_hooks::middleware::HookedLoopCapabilityPort;
        use ironclaw_hooks::registry::HookRegistry;
        use ironclaw_host_api::{CapabilityId, Resolution, ResolutionBatch};
        use ironclaw_turns::run_profile::{
            AgentLoopHostError, CapabilityBatchInvocation, CapabilityFailureKind,
            CapabilityInputRef, CapabilityInvocation, CapabilitySurfaceVersion, LoopCapabilityPort,
            VisibleCapabilityRequest, VisibleCapabilitySurface, resolution,
        };
        use ironclaw_turns::{CapabilityActivityId, LoopGateRef};

        struct SequencedSpawnPort {
            calls: AtomicUsize,
            outcomes: Mutex<VecDeque<Resolution>>,
        }

        #[async_trait::async_trait]
        impl LoopCapabilityPort for SequencedSpawnPort {
            async fn visible_capabilities(
                &self,
                _request: VisibleCapabilityRequest,
            ) -> Result<VisibleCapabilitySurface, AgentLoopHostError> {
                unreachable!("the guard middleware test does not query the capability surface")
            }

            async fn invoke_capability(
                &self,
                _request: CapabilityInvocation,
            ) -> Result<Resolution, AgentLoopHostError> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(self
                    .outcomes
                    .lock()
                    .expect("outcome queue")
                    .pop_front()
                    .expect("scripted outcome"))
            }

            async fn invoke_capability_batch(
                &self,
                _request: CapabilityBatchInvocation,
            ) -> Result<ResolutionBatch, AgentLoopHostError> {
                unreachable!("single-call regression does not invoke a batch")
            }
        }

        fn invocation(capability_name: &str, input_label: &str) -> CapabilityInvocation {
            CapabilityInvocation {
                activity_id: CapabilityActivityId::new(),
                surface_version: CapabilitySurfaceVersion::new("tianquan-guard:test")
                    .expect("valid surface version"),
                capability_id: CapabilityId::new(capability_name).expect("valid capability id"),
                input_ref: CapabilityInputRef::new(format!("input:{input_label}"))
                    .expect("valid input ref"),
                approval_resume: None,
                auth_resume: None,
            }
        }

        let inner = Arc::new(SequencedSpawnPort {
            calls: AtomicUsize::new(0),
            outcomes: Mutex::new(VecDeque::from([
                resolution::failed(
                    CapabilityFailureKind::Transient,
                    "scope recovery in progress".to_string(),
                    None,
                ),
                resolution::approval_required(
                    LoopGateRef::new("gate:spawn-waiting").expect("valid gate"),
                    "waiting for child".to_string(),
                    None,
                )
                .resolution,
            ])),
        });
        let dispatcher = install_tianquan_guard(HookDispatcherBuilder::new(HookRegistry::new()))
            .expect("install guard pair")
            .build_arc();
        let wrapped = HookedLoopCapabilityPort::new(
            inner.clone(),
            dispatcher,
            TenantId::new("tianquan-test").expect("valid tenant"),
        );

        let first = wrapped
            .invoke_capability(invocation(CAPABILITY_SPAWN_SUBAGENT, "first"))
            .await
            .expect("transient is a resolution");
        let Resolution::Done(first) = first else {
            panic!("expected transient Done resolution");
        };
        assert_eq!(
            first.verdict.error_kind(),
            Some(&ironclaw_host_api::FailureKind::Transient)
        );

        let retry = wrapped
            .invoke_capability(invocation(CAPABILITY_SPAWN_SUBAGENT, "retry"))
            .await
            .expect("immediate retry reaches inner");
        assert!(retry.parks(), "the successful dispatch must park");

        assert!(matches!(
            wrapped
                .invoke_capability(invocation(CAPABILITY_SPAWN_SUBAGENT, "duplicate"))
                .await
                .expect("duplicate is model-visible denial"),
            Resolution::Denied(_)
        ));
        assert!(matches!(
            wrapped
                .invoke_capability(invocation(CAPABILITY_RESULT_READ, "poll"))
                .await
                .expect("poll is model-visible denial"),
            Resolution::Denied(_)
        ));
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            2,
            "transient retry reaches inner, but waiting-state duplicate and poll do not"
        );
    }

    #[test]
    fn install_into_builder_succeeds() {
        // The install step must succeed against a fresh builder (the same call
        // the composition root validates against a scratch builder and replays
        // per run).
        use ironclaw_hooks::dispatch::HookDispatcherBuilder;
        use ironclaw_hooks::registry::{HookPointSpec, HookRegistry};
        let builder = HookDispatcherBuilder::new(HookRegistry::new());
        let built = install_tianquan_guard(builder).expect("install must succeed");
        let dispatcher = built.build_arc();
        let hook_id = HookId::for_builtin(TIANQUAN_GUARD_CANONICAL_PATH, HookVersion::ONE);
        let bindings = dispatcher.active_bindings_snapshot(HookPointSpec::BeforeCapability);
        assert!(
            bindings.iter().any(|b| b.hook_id == hook_id),
            "guard must be bound at BeforeCapability; saw {bindings:?}"
        );
        let observer_id =
            HookId::for_builtin(TIANQUAN_GUARD_OBSERVER_CANONICAL_PATH, HookVersion::ONE);
        let observer_bindings = dispatcher.active_bindings_snapshot(HookPointSpec::AfterCapability);
        assert!(
            observer_bindings.iter().any(|b| b.hook_id == observer_id),
            "guard must share lifecycle state through an AfterCapability observer; saw \
             {observer_bindings:?}"
        );
    }

    #[test]
    fn needs_input_is_false() {
        // The guard never reads arguments; opting out of input lets the
        // middleware skip eager resolution. This is a load-bearing property
        // (see PrivilegedBeforeCapabilityHook::needs_input docs).
        let guard = TianquanBuiltinGuard::new();
        assert!(!guard.needs_input());
    }

    #[test]
    fn mutex_poison_denies_fail_closed() {
        // If the state mutex is poisoned (another invocation panicked
        // mid-evaluation), the guard must fail closed: deny rather than let the
        // anti-pattern proceed while state is unreadable. The only way to
        // poison a std `Mutex` is a panic while it is held, so we do that in a
        // `catch_unwind` and then evaluate through a guard sharing the poisoned
        // state.
        let poisoned: Arc<Mutex<TianquanGuardState>> =
            Arc::new(Mutex::new(TianquanGuardState::new()));
        let poisoned_clone = Arc::clone(&poisoned);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned_clone.lock().expect("lock for poison");
            panic!("intentional poison of tianquan guard test mutex");
        }));
        // The next `lock()` on `poisoned` returns `Err(PoisonError)`.
        assert!(poisoned.is_poisoned(), "mutex should be poisoned");
        let poisoned_guard = TianquanBuiltinGuard { state: poisoned };
        let outcome = evaluate(&poisoned_guard, &ctx_for(CAPABILITY_RESULT_READ));
        assert_denied(outcome);
    }
}
