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
//! # State lifetime & segment reset (verified 2026-07-31)
//!
//! The context does not carry a `run_id` / `turn_id` (verified against
//! `ironclaw_hooks::points::capability`), but the guard's effective state
//! lifetime is **one continuous execution segment of a run**, not the whole
//! run: `RebornTurnRunExecutor` calls `HostFactory::create_host(claimed)` each
//! time the scheduler (re-)claims a run, and each host build mints a fresh
//! dispatcher (see `factory::build_hook_dispatcher_builder_factory_with`), so
//! each segment gets a fresh guard. When a blocking spawn parks the run and
//! the child's terminal event settles it, the resume re-claims the run and
//! builds a new host — **`pending_spawn` clears implicitly at the resume
//! boundary**. This is the precise "spawn completed" reset the original design
//! wanted as a TODO: it already exists structurally. Live verification
//! (verify_l3 2026-07-31, thread d36377fa): post-resume `result_read` calls
//! 78 s after spawn (child completed) were correctly allowed — no over-deny of
//! legitimate result reads — while pre-resume polls within a segment are
//! denied for the full `PENDING_SPAWN_WINDOW`.
//!
//! - `pending_spawn` also clears itself on a **time window** as a segment-
//!   internal backstop: a spawn is considered "pending" for
//!   `PENDING_SPAWN_WINDOW` (300 s) after the spawn is allowed. The window is
//!   sized to cover worst-case subagent generation within one segment: minimax
//!   needs ~115 s for a 3000-char chapter (measured 2026-07-29), the request
//!   timeout is 180 s and the runner lease is 200 s (a segment cannot outlive
//!   its lease by much), so 300 s ≈ the rest of the segment for any spawn that
//!   has time to run at all. The previous 60 s window expired mid-generation
//!   and re-allowed `result_read` polling (handover 2026-07-31 下轮首做①).
//! - `spawn_count` / `shell_count` are cumulative for the lifetime of the
//!   guard instance (one segment). Cross-segment leaks cannot occur; per-turn
//!   accumulation across resume boundaries is intentionally NOT enforced
//!   (post-resume re-spawn and result_read are legitimate).
//!
//! Note: an `AfterCapability` observer cannot serve as the completion signal:
//! `ObserverHookContext` carries no capability name, and `spawn_subagent`
//! returns a `spawned` handle immediately (the blocking is at the loop-resume
//! level), so its completion event fires long before the child finishes. The
//! segment-boundary reset + time window is the correct mechanism.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use ironclaw_hooks::identity::{HookId, HookVersion};
use ironclaw_hooks::ordering::HookPhase;
use ironclaw_hooks::points::BeforeCapabilityHookContext;
use ironclaw_hooks::sink::{PrivilegedBeforeCapabilityHook, PrivilegedGateSink};

use crate::error::RebornBuildError;

/// Canonical identity path for the Tianquan builtin guard. Stable across runs
/// so the binding is deterministic for checkpoint replay validation.
pub(crate) const TIANQUAN_GUARD_CANONICAL_PATH: &str =
    "ironclaw_reborn_composition::hooks::tianquan_guard::TianquanBuiltinGuard";

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

/// Cross-call state for [`TianquanBuiltinGuard`]. Held behind an
/// `Arc<Mutex<...>>` so the guard (which is `Send + Sync + 'static`) can share
/// mutable counters across invocations within one dispatcher's lifetime.
#[derive(Debug, Clone)]
struct TianquanGuardState {
    /// True when a spawn was allowed and the time window has not yet elapsed.
    /// Set on the first allowed spawn; cleared by the time-window check on the
    /// next relevant evaluation.
    pending_spawn: bool,
    /// `Instant` at which the last allowed spawn fired. Used to expire
    /// `pending_spawn` after [`PENDING_SPAWN_WINDOW`].
    last_spawn_at: Option<Instant>,
    /// Cumulative count of `builtin.spawn_subagent` evaluations seen by this
    /// guard (both allowed and denied). Retained for observability/tests; the
    /// duplicate-spawn deny keys on `pending_spawn` (window-active), not on
    /// this counter, so a legitimate re-spawn after the previous child's
    /// window expired is allowed.
    spawn_count: u32,
    /// Cumulative count of `builtin.shell` evaluations seen by this guard.
    /// Denied over-limit calls still increment so a storm of shell calls keeps
    /// the guard in "rate-limited" mode rather than flickering.
    shell_count: u32,
}

impl TianquanGuardState {
    fn new() -> Self {
        Self {
            pending_spawn: false,
            last_spawn_at: None,
            spawn_count: 0,
            shell_count: 0,
        }
    }

    /// Expire `pending_spawn` if the time window has elapsed. Called at the
    /// top of every evaluation so the deny on `result_read` is lifted promptly
    /// once the window closes.
    fn expire_pending_spawn(&mut self, now: Instant) {
        if self.pending_spawn
            && let Some(spawned_at) = self.last_spawn_at
            && now.duration_since(spawned_at) >= PENDING_SPAWN_WINDOW
        {
            self.pending_spawn = false;
            self.last_spawn_at = None;
        }
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

    /// Snapshot-test accessor for the pending-spawn flag. Test-only: used to
    /// assert that the first allowed spawn sets the flag.
    #[cfg(test)]
    fn pending_spawn(&self) -> bool {
        self.state
            .lock()
            .expect("Tianquan guard state mutex poisoned")
            .pending_spawn
    }
}

impl Default for TianquanBuiltinGuard {
    fn default() -> Self {
        Self::new()
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
        state.expire_pending_spawn(now);

        match ctx.capability_name.as_str() {
            CAPABILITY_SPAWN_SUBAGENT => {
                state.spawn_count = state.spawn_count.saturating_add(1);
                if state.pending_spawn {
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
                // No spawn pending: record the window start and allow.
                state.pending_spawn = true;
                state.last_spawn_at = Some(now);
                drop(state);
                sink.pass();
            }
            CAPABILITY_RESULT_READ => {
                if state.pending_spawn {
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
            // Any other capability: the guard has no opinion. `pass` lets the
            // composed decision proceed under the other hooks' authority.
            _ => {
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
    let hook_id = HookId::for_builtin(TIANQUAN_GUARD_CANONICAL_PATH, HookVersion::ONE);
    builder
        .install_builtin_before_capability(
            hook_id,
            HookPhase::Policy,
            Box::new(TianquanBuiltinGuard::new()),
        )
        .map_err(|error| RebornBuildError::InvalidConfig {
            reason: format!("failed to install tianquan builtin guard hook: {error}"),
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
        // First spawn: allowed, sets pending_spawn.
        let spawn_outcome = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_passed(spawn_outcome);
        assert!(guard.pending_spawn(), "first spawn must set pending_spawn");
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
    fn respawn_allowed_after_window_expiry() {
        // The duplicate-spawn deny keys on the pending window, not a
        // cumulative count: once the window has elapsed (child presumed
        // resumed, or hung past recovery), a fresh spawn must be allowed so
        // legitimate re-spawn (validator blocked the first child's prose, or
        // the child hung) is not permanently blocked.
        //
        // `Instant` cannot be advanced, so we expire the window the same way
        // `expire_pending_spawn` would once `now - last_spawn_at >= window`,
        // then assert the public behavior (spawn passes again).
        let guard = TianquanBuiltinGuard::new();
        evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        let denied = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_denied(denied);

        {
            let mut state = guard
                .state
                .lock()
                .expect("Tianquan guard state mutex poisoned");
            state.pending_spawn = false;
            state.last_spawn_at = None;
        }

        let respawn = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_passed(respawn);
        assert!(
            guard.pending_spawn(),
            "re-spawn must re-arm the pending window"
        );
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
        assert!(!guard.pending_spawn());
        let outcome = evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert_passed(outcome);
        assert!(guard.pending_spawn(), "first spawn must set pending_spawn");
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
        // Because `Instant` cannot be arbitrarily advanced, we directly
        // manipulate the state the same way `expire_pending_spawn` would once
        // `now - last_spawn_at >= window`, then assert the public behavior
        // (result_read passes). This pins the contract: once the window
        // closes, the pending-spawn deny lifts.
        let guard = TianquanBuiltinGuard::new();
        evaluate(&guard, &ctx_for(CAPABILITY_SPAWN_SUBAGENT));
        assert!(guard.pending_spawn());

        {
            let mut state = guard
                .state
                .lock()
                .expect("Tianquan guard state mutex poisoned");
            state.pending_spawn = false;
            state.last_spawn_at = None;
        }

        let read_outcome = evaluate(&guard, &ctx_for(CAPABILITY_RESULT_READ));
        assert_passed(read_outcome);
    }

    #[test]
    fn other_capability_passes() {
        // A capability the guard does not key on must pass (no opinion),
        // proving the guard is inert for the rest of the capability surface.
        let guard = TianquanBuiltinGuard::new();
        let outcome = evaluate(&guard, &ctx_for("builtin.some_other_tool"));
        assert_passed(outcome);
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
