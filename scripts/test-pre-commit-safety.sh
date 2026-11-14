#!/usr/bin/env bash
# Regression tests for the grep pipelines in `pre-commit-safety.sh`.
#
# The PROJECTION / DISPATCH / CREDNAME checks all pipe through
# `grep -nE '^\+' | grep -E <positive> | grep -vE <exclusions>`.
# A previous version of the exclusion regex used `^\+\+\+` to filter
# diff header lines (`+++ b/file.rs`), which silently never fired —
# `grep -n` prepends a `N:` line-number prefix, so `^` no longer
# anchors against the `+++` bytes. This test locks in the corrected
# `:\+\+\+ ` shape.

set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

PASS=0
FAIL=0

assert_filtered() {
    local label="$1" input="$2" positive="$3" exclusions="$4"
    # Emulate the production pipeline: `grep -n '^+'` adds the line-number
    # prefix, then positive/negative filters run against that shape.
    local result
    if result=$(printf '%s\n' "$input" \
        | grep -nE '^\+' \
        | grep -E "$positive" \
        | grep -vE "$exclusions" \
        | head -5 || true); then :; fi
    if [ -z "${result:-}" ]; then
        echo "OK: $label (correctly filtered)"
        PASS=$((PASS + 1))
    else
        echo "FAIL: $label — line leaked past exclusions:"
        echo "$result" | sed 's/^/    /'
        FAIL=$((FAIL + 1))
    fi
}

assert_flagged() {
    local label="$1" input="$2" positive="$3" exclusions="$4"
    local result
    if result=$(printf '%s\n' "$input" \
        | grep -nE '^\+' \
        | grep -E "$positive" \
        | grep -vE "$exclusions" \
        | head -5 || true); then :; fi
    if [ -n "${result:-}" ]; then
        echo "OK: $label (correctly flagged)"
        PASS=$((PASS + 1))
    else
        echo "FAIL: $label — line not flagged by positive pattern"
        FAIL=$((FAIL + 1))
    fi
}

assert_precommit_blocks() {
    local label="$1" path="$2" base_content="$3" changed_content="$4" expected="$5"
    local tmp output status
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/precommit-safety.XXXXXX")
    set +e
    (
        cd "$tmp"
        git init -q
        git config user.email test@example.com
        git config user.name "Test User"
        mkdir -p "$(dirname "$path")"
        printf '%b' "$base_content" > "$path"
        git add "$path"
        git commit -qm base
        printf '%b' "$changed_content" > "$path"
        git add "$path"
        set +e
        output=$("$ROOT_DIR/scripts/pre-commit-safety.sh" 2>&1)
        status=$?
        set -e
        if [ "$status" -ne 0 ] && printf '%s\n' "$output" | grep -Fq "$expected"; then
            echo "OK: $label (pre-commit blocked)"
            exit 0
        fi
        echo "FAIL: $label — expected block containing '$expected', got status $status"
        printf '%s\n' "$output" | sed 's/^/    /'
        exit 1
    )
    status=$?
    set -e
    rm -rf "$tmp"
    if [ "$status" -eq 0 ]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
    fi
}

assert_precommit_allows() {
    local label="$1" path="$2" base_content="$3" changed_content="$4"
    local tmp output status
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/precommit-safety.XXXXXX")
    set +e
    (
        cd "$tmp"
        git init -q
        git config user.email test@example.com
        git config user.name "Test User"
        mkdir -p "$(dirname "$path")"
        printf '%b' "$base_content" > "$path"
        git add "$path"
        git commit -qm base
        printf '%b' "$changed_content" > "$path"
        git add "$path"
        set +e
        output=$("$ROOT_DIR/scripts/pre-commit-safety.sh" 2>&1)
        status=$?
        set -e
        if [ "$status" -eq 0 ]; then
            echo "OK: $label (pre-commit allowed)"
            exit 0
        fi
        echo "FAIL: $label — expected allow, got status $status"
        printf '%s\n' "$output" | sed 's/^/    /'
        exit 1
    )
    status=$?
    set -e
    rm -rf "$tmp"
    if [ "$status" -eq 0 ]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
    fi
}

assert_precommit_allows_with_unstaged() {
    local label="$1" path="$2" base_content="$3" staged_content="$4" unstaged_content="$5"
    local tmp output status
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/precommit-safety.XXXXXX")
    set +e
    (
        cd "$tmp"
        git init -q
        git config user.email test@example.com
        git config user.name "Test User"
        mkdir -p "$(dirname "$path")"
        printf '%b' "$base_content" > "$path"
        git add "$path"
        git commit -qm base
        printf '%b' "$staged_content" > "$path"
        git add "$path"
        printf '%b' "$unstaged_content" > "$path"
        set +e
        output=$("$ROOT_DIR/scripts/pre-commit-safety.sh" 2>&1)
        status=$?
        set -e
        if [ "$status" -eq 0 ]; then
            echo "OK: $label (pre-commit allowed)"
            exit 0
        fi
        echo "FAIL: $label — expected allow, got status $status"
        printf '%s\n' "$output" | sed 's/^/    /'
        exit 1
    )
    status=$?
    set -e
    rm -rf "$tmp"
    if [ "$status" -eq 0 ]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
    fi
}

# Rust commonly keeps large unit-test modules in a sibling `*_tests.rs` file
# that is included by a `#[cfg(test)] mod ...;` declaration. Those files are
# test-only even though they do not live under a `tests/` directory.
assert_precommit_allows "PANIC: sibling *_tests.rs files are test-only" \
    "src/auth_tests.rs" \
    "fn existing_test() {}\n" \
    "fn existing_test() {}\n#[test]\nfn new_test() { Result::<(), &str>::Ok(()).expect(\"fixture\"); }\n"

assert_precommit_allows "PANIC: sibling test_*.rs files are test-only" \
    "src/test_auth.rs" \
    "fn existing_test() {}\n" \
    "fn existing_test() {}\n#[test]\nfn new_test() { Result::<(), &str>::Ok(()).expect(\"fixture\"); }\n"

assert_precommit_allows "PANIC: sibling *_test.rs files are test-only" \
    "src/auth_test.rs" \
    "fn existing_test() {}\n" \
    "fn existing_test() {}\n#[test]\nfn new_test() { Result::<(), &str>::Ok(()).expect(\"fixture\"); }\n"

assert_precommit_allows_unstaged_diff() {
    local label="$1" path="$2" base_content="$3" changed_content="$4"
    local tmp output status
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/precommit-safety.XXXXXX")
    set +e
    (
        cd "$tmp"
        git init -q
        git config user.email test@example.com
        git config user.name "Test User"
        mkdir -p "$(dirname "$path")"
        printf '%b' "$base_content" > "$path"
        git add "$path"
        git commit -qm base
        git branch -M main
        printf '%b' "$changed_content" > "$path"
        set +e
        output=$("$ROOT_DIR/scripts/pre-commit-safety.sh" 2>&1)
        status=$?
        set -e
        if [ "$status" -eq 0 ]; then
            echo "OK: $label (standalone diff allowed)"
            exit 0
        fi
        echo "FAIL: $label — expected allow, got status $status"
        printf '%s\n' "$output" | sed 's/^/    /'
        exit 1
    )
    status=$?
    set -e
    rm -rf "$tmp"
    if [ "$status" -eq 0 ]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
    fi
}

assert_staged_scope_ignores_committed_dispatch() {
    local tmp output status
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/precommit-safety.XXXXXX")
    set +e
    (
        cd "$tmp"
        git init -q
        git config user.email test@example.com
        git config user.name "Test User"
        mkdir -p src/cli crates
        printf 'fn existing() {}\n' > src/cli/commands.rs
        git add src/cli/commands.rs
        git commit -qm base
        git branch -M main
        git checkout -qb feature
        printf 'fn existing() { let store = state.store.as_ref(); }\n' > src/cli/commands.rs
        git add src/cli/commands.rs
        git commit -qm older-feature-change
        printf 'fn staged_change() {}\n' > crates/staged.rs
        git add crates/staged.rs
        output=$("$ROOT_DIR/scripts/pre-commit-safety.sh" 2>&1)
        status=$?
        if [ "$status" -eq 0 ]; then
            echo "OK: staged scope ignores earlier branch dispatch edits"
            exit 0
        fi
        echo "FAIL: staged scope inspected earlier branch diff"
        printf '%s\n' "$output" | sed 's/^/    /'
        exit 1
    )
    status=$?
    set -e
    case "$tmp" in
        "${TMPDIR:-/tmp}"/precommit-safety.*) rm -rf -- "$tmp" ;;
        *) echo "FAIL: unsafe test temp path $tmp"; exit 1 ;;
    esac
    if [ "$status" -eq 0 ]; then
        PASS=$((PASS + 1))
    else
        FAIL=$((FAIL + 1))
    fi
}

assert_staged_scope_ignores_committed_dispatch

# ── PROJECTION ────────────────────────────────────────────────
# Positive: any `.broadcast_for_user(` (SseManager-unique method) or
#           `sse.broadcast(` with a portable word boundary.
# Exclusions: `// projection-exempt: <category>, <detail>`, `// safety:`,
#             and diff-header lines (`+++ b/path`) via `:\+\+\+ `.
PROJ_POS='(\.broadcast_for_user|(^|[^[:alnum:]_])sse\.broadcast)[[:space:]]*\('
PROJ_NEG='// projection-exempt: [^,]+,[[:space:]]*[^[:space:]]|// safety:|:\+\+\+ '

# Diff header lines must be filtered.
assert_filtered "PROJECTION: diff header line is filtered" \
    "+++ b/src/bridge/router.rs" \
    "$PROJ_POS" \
    "$PROJ_NEG"

# A real broadcast call is flagged.
assert_flagged "PROJECTION: bare sse.broadcast_for_user is flagged" \
    "+    sse.broadcast_for_user(&user, event);" \
    "$PROJ_POS" \
    "$PROJ_NEG"

# Chained receiver (state.sse.broadcast_for_user) is flagged.
assert_flagged "PROJECTION: chained state.sse.broadcast_for_user is flagged" \
    "+    state.sse.broadcast_for_user(&user, event);" \
    "$PROJ_POS" \
    "$PROJ_NEG"

# A rustfmt-wrapped call is flagged.
assert_flagged "PROJECTION: rustfmt-wrapped .broadcast_for_user is flagged" \
    "+    .broadcast_for_user(&user, event);" \
    "$PROJ_POS" \
    "$PROJ_NEG"

# Non-`sse` receiver must still fire — `broadcast_for_user` is unique to
# SseManager, so the method name alone is authoritative.
assert_flagged "PROJECTION: non-sse receiver .broadcast_for_user is flagged" \
    "+    manager.broadcast_for_user(&user, event);" \
    "$PROJ_POS" \
    "$PROJ_NEG"

# Plain sse.broadcast call is flagged via the portable word boundary.
assert_flagged "PROJECTION: bare sse.broadcast is flagged" \
    "+    sse.broadcast(event);" \
    "$PROJ_POS" \
    "$PROJ_NEG"

# The portable boundary must not fire on a longer identifier that ends
# in 'sse' (e.g. `usse.broadcast(...)` — not a real SseManager).
assert_filtered "PROJECTION: identifier ending in sse is not flagged" \
    "+    usse.broadcast(event);" \
    "$PROJ_POS" \
    "$PROJ_NEG"

# Correctly annotated call is exempted.
assert_filtered "PROJECTION: annotated call with category+detail is exempt" \
    "+    sse.broadcast_for_user(&user, event); // projection-exempt: bridge dispatcher, auth gate" \
    "$PROJ_POS" \
    "$PROJ_NEG"

# Bare `// projection-exempt: legacy` (no comma, no detail) does NOT exempt.
assert_flagged "PROJECTION: unnamed 'legacy' suppression still flagged" \
    "+    sse.broadcast_for_user(&user, event); // projection-exempt: legacy" \
    "$PROJ_POS" \
    "$PROJ_NEG"

# Empty detail after the comma (`// projection-exempt: foo,`) does NOT
# exempt — the documented format requires a non-empty detail.
assert_flagged "PROJECTION: empty detail after comma still flagged" \
    "+    sse.broadcast_for_user(&user, event); // projection-exempt: foo," \
    "$PROJ_POS" \
    "$PROJ_NEG"

# Trailing whitespace after the comma without a detail also does NOT exempt.
assert_flagged "PROJECTION: comma + whitespace-only detail still flagged" \
    "+    sse.broadcast_for_user(&user, event); // projection-exempt: foo,   " \
    "$PROJ_POS" \
    "$PROJ_NEG"

# ── DISPATCH ──────────────────────────────────────────────────
DISPATCH_POS='state\.(store|workspace|workspace_pool|extension_manager|skill_registry|session_manager)\.'
DISPATCH_NEG='// dispatch-exempt:|// safety:|:\+\+\+ '

assert_filtered "DISPATCH: diff header line is filtered" \
    "+++ b/src/channels/web/handlers/foo.rs" \
    "$DISPATCH_POS" \
    "$DISPATCH_NEG"

assert_flagged "DISPATCH: direct state.store touch is flagged" \
    "+    state.store.create_project(...)" \
    "$DISPATCH_POS" \
    "$DISPATCH_NEG"

# ── CREDNAME ──────────────────────────────────────────────────
# Portable word boundary: `(^|[^[:alnum:]_])` / `([^[:alnum:]_]|$)` —
# `grep -E`'s `\b` is a GNU extension and not recognised by BSD grep.
CREDNAME_POS='(^|[^[:alnum:]_])CredentialName([^[:alnum:]_]|$)'
CREDNAME_NEG='// web-identity-exempt:|// safety:|:\+\+\+ '

assert_filtered "CREDNAME: diff header line is filtered" \
    "+++ b/src/channels/web/features/settings.rs" \
    "$CREDNAME_POS" \
    "$CREDNAME_NEG"

# A similarly-named but distinct identifier must not fire.
assert_filtered "CREDNAME: CredentialNameExt (different type) is not flagged" \
    "+    let ext: CredentialNameExt = ...;" \
    "$CREDNAME_POS" \
    "$CREDNAME_NEG"

assert_flagged "CREDNAME: bare CredentialName reference is flagged" \
    "+    let name: CredentialName = ...;" \
    "$CREDNAME_POS" \
    "$CREDNAME_NEG"

# ── MULTITENANT ───────────────────────────────────────────────
# A new unscoped `sse.broadcast(...)` must either be transport-only or
# carry an explicit `// multi-tenant-safe: <reason>` annotation.
# `broadcast_for_user(...)` is the safe path and must be exempt.
MT_POS='(^|[^[:alnum:]_])sse\.broadcast[[:space:]]*\('
MT_NEG='\.broadcast_for_user|// projection-exempt: transport-only,[[:space:]]*[^[:space:]]|//.*multi-tenant-safe: [^[:space:]]|// safety:|:\+\+\+ '

assert_filtered "MULTITENANT: diff header line is filtered" \
    "+++ b/src/extensions/manager.rs" \
    "$MT_POS" \
    "$MT_NEG"

assert_filtered "MULTITENANT: broadcast_for_user is exempt" \
    "+    sse.broadcast_for_user(&user, event);" \
    "$MT_POS" \
    "$MT_NEG"

assert_filtered "MULTITENANT: heartbeat (transport-only) is exempt" \
    "+    sse.broadcast(AppEvent::Heartbeat); // projection-exempt: transport-only, heartbeat" \
    "$MT_POS" \
    "$MT_NEG"

# Receiver-prefixed call sites: the boundary regex matches `.sse.broadcast(`
# because the leading `.` is non-alnum-and-non-underscore, so the existing
# check covers production patterns like `state.sse.broadcast(`,
# `gw_state.sse.broadcast(`, and rustfmt-wrapped chains. These tests pin
# that behaviour against a future regex tightening.
assert_flagged "MULTITENANT: state.sse.broadcast (receiver-prefixed) is flagged" \
    "+    state.sse.broadcast(event);" \
    "$MT_POS" \
    "$MT_NEG"

assert_flagged "MULTITENANT: gw_state.sse.broadcast (snake_case receiver) is flagged" \
    "+    gw_state.sse.broadcast(event);" \
    "$MT_POS" \
    "$MT_NEG"

assert_filtered "MULTITENANT: state.sse.broadcast with annotation is exempt" \
    "+    state.sse.broadcast(event); // multi-tenant-safe: single-tenant fallback" \
    "$MT_POS" \
    "$MT_NEG"

assert_filtered "MULTITENANT: explicit multi-tenant-safe annotation is exempt" \
    "+    sse.broadcast(event); // multi-tenant-safe: only reached when multi_tenant_mode=false" \
    "$MT_POS" \
    "$MT_NEG"

# Compound annotation: a single `// ` comment can carry both
# `projection-exempt:` and `multi-tenant-safe:` because Rust line
# comments don't nest. The marker scanner must accept either marker
# anywhere in the trailing comment, not only when the comment opens
# with it. See `src/channels/web/mod.rs::dispatch_status_event` and
# `src/main.rs` sandbox JobEvent dispatcher.
assert_filtered "MULTITENANT: compound projection-exempt + multi-tenant-safe annotation is exempt" \
    "+    sse.broadcast(event); // projection-exempt: bridge dispatcher, single-tenant unscoped status; multi-tenant-safe: only reached when multi_tenant_mode=false" \
    "$MT_POS" \
    "$MT_NEG"

assert_flagged "MULTITENANT: bare unscoped sse.broadcast is flagged" \
    "+    sse.broadcast(event);" \
    "$MT_POS" \
    "$MT_NEG"

assert_flagged "MULTITENANT: unscoped broadcast with bridge-dispatcher projection-exempt is still flagged" \
    "+    sse.broadcast(event); // projection-exempt: bridge dispatcher, status update" \
    "$MT_POS" \
    "$MT_NEG"

assert_flagged "MULTITENANT: unscoped broadcast with empty multi-tenant-safe detail is still flagged" \
    "+    sse.broadcast(event); // multi-tenant-safe: " \
    "$MT_POS" \
    "$MT_NEG"

# ── ARCH-SPRAWL integration checks ────────────────────────────
assert_precommit_blocks "ARCH-SPRAWL: too_many_arguments allow needs arch-exempt plan" \
    "src/runtime.rs" \
    "fn existing() {}\n" \
    "#[allow(clippy::too_many_arguments)]\nfn execute(a: u8, b: u8, c: u8, d: u8, e: u8, f: u8, g: u8, h: u8) {}\n" \
    "ARCH-SPRAWL"

assert_precommit_allows "ARCH-SPRAWL: annotated too_many_arguments allow is accepted" \
    "src/runtime.rs" \
    "fn existing() {}\n" \
    "// arch-exempt: too_many_args, needs RuntimeInputs bundle, plan #2800\n#[allow(clippy::too_many_arguments)]\nfn execute(a: u8, b: u8, c: u8, d: u8, e: u8, f: u8, g: u8, h: u8) {}\n"

assert_precommit_allows "ARCH-SPRAWL: exemption reason may contain commas" \
    "src/runtime.rs" \
    "fn existing() {}\n" \
    "// arch-exempt: too_many_args, split struct, add builder, plan #2800\n#[allow(clippy::too_many_arguments)]\nfn execute(a: u8, b: u8, c: u8, d: u8, e: u8, f: u8, g: u8, h: u8) {}\n"

assert_precommit_blocks "ARCH-SPRAWL: optional Arc plus with-builder needs arch-exempt plan" \
    "src/runtime.rs" \
    "fn existing() {}\n" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime {\n    baz: Option<Arc<dyn Baz>>,\n}\nimpl Runtime {\n    fn with_baz(mut self, baz: Arc<dyn Baz>) -> Self {\n        self.baz = Some(baz);\n        self\n    }\n}\n" \
    "ARCH-SPRAWL"

assert_precommit_allows "ARCH-SPRAWL: annotated optional Arc plus with-builder is accepted" \
    "src/runtime.rs" \
    "fn existing() {}\n" \
    "use std::sync::Arc;\ntrait Baz {}\n// arch-exempt: optional_arc, feature-gated runtime adapter, plan #2800\nstruct Runtime {\n    baz: Option<Arc<dyn Baz>>,\n}\nimpl Runtime {\n    fn with_baz(mut self, baz: Arc<dyn Baz>) -> Self {\n        self.baz = Some(baz);\n        self\n    }\n}\n"

assert_precommit_allows "ARCH-SPRAWL: optional Arc and unrelated with-builder in separate hunks are allowed" \
    "src/runtime.rs" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime {\n}\n\n\n\n\n\n\n\n\nimpl Runtime {\n}\n" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime {\n    baz: Option<Arc<dyn Baz>>,\n}\n\n\n\n\n\n\n\n\nimpl Runtime {\n    fn with_cache(mut self, enabled: bool) -> Self {\n        self\n    }\n}\n"

assert_precommit_blocks "ARCH-SPRAWL: same-name optional Arc and with-builder in separate hunks still block" \
    "src/runtime.rs" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime {\n}\n\n\n\n\n\n\n\n\nimpl Runtime {\n}\n" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime {\n    baz: Option<Arc<dyn Baz>>,\n}\n\n\n\n\n\n\n\n\nimpl Runtime {\n    fn with_baz(mut self, baz: Arc<dyn Baz>) -> Self {\n        self.baz = Some(baz);\n        self\n    }\n}\n" \
    "ARCH-SPRAWL"

assert_precommit_allows "ARCH-SPRAWL: annotated optional Arc and with-builder in separate hunks are accepted" \
    "src/runtime.rs" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime {\n}\n\n\n\n\n\n\n\n\nimpl Runtime {\n}\n" \
    "use std::sync::Arc;\ntrait Baz {}\n// arch-exempt: optional_arc, feature-gated runtime adapter, plan #2800\nstruct Runtime {\n    baz: Option<Arc<dyn Baz>>,\n}\n\n\n\n\n\n\n\n\nimpl Runtime {\n    fn with_baz(mut self, baz: Arc<dyn Baz>) -> Self {\n        self.baz = Some(baz);\n        self\n    }\n}\n"

assert_precommit_allows "ARCH-SPRAWL: optional Arc and unrelated with-builder in same hunk are allowed" \
    "src/runtime.rs" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime {\n}\nimpl Runtime {\n}\n" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime {\n    baz: Option<Arc<dyn Baz>>,\n}\nimpl Runtime {\n    fn with_cache(mut self, enabled: bool) -> Self {\n        self\n    }\n}\n"

assert_precommit_allows_unstaged_diff "ARCH-SPRAWL: existing optional Arc pair near unrelated edit is allowed" \
    "src/runtime.rs" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime { baz: Option<Arc<dyn Baz>> }\nimpl Runtime { fn with_baz(mut self, baz: Arc<dyn Baz>) -> Self { self } }\n" \
    "use std::sync::Arc;\ntrait Baz {}\nstruct Runtime { baz: Option<Arc<dyn Baz>> }\nimpl Runtime { fn with_baz(mut self, baz: Arc<dyn Baz>) -> Self { self } }\nfn unrelated() {}\n"

large_base=""
for i in $(seq 1 1501); do
    large_base="${large_base}// existing ${i}
"
done
assert_precommit_blocks "ARCH-SPRAWL: large file additions need arch-exempt plan" \
    "src/large.rs" \
    "$large_base" \
    "${large_base}// added line
" \
    "ARCH-SPRAWL"

large_base_with_exempt="// arch-exempt: large_file, tracked decomposition, plan #2800
${large_base}"
assert_precommit_allows "ARCH-SPRAWL: existing large-file exemption covers later edits" \
    "src/large.rs" \
    "$large_base_with_exempt" \
    "${large_base_with_exempt}// added line
"

assert_precommit_allows_with_unstaged "ARCH-SPRAWL: large-file check uses staged content, not unstaged working tree" \
    "src/large.rs" \
    "$large_base_with_exempt" \
    "${large_base_with_exempt}// staged added line
" \
    "${large_base}// unstaged edit removed exemption
"

assert_precommit_allows "ARCH-SPRAWL: single dispatcher call is allowed" \
    "src/runtime.rs" \
    "fn existing() {}\n" \
    "fn execute(dispatcher: &Dispatcher, request: Request) {\n    dispatcher.dispatch(request);\n}\n"

assert_precommit_blocks "ARCH-SPRAWL: repeated dispatcher calls need arch-exempt plan" \
    "src/runtime.rs" \
    "fn existing() {}\n" \
    "fn execute_one(dispatcher: &Dispatcher, request: Request) {\n    dispatcher.dispatch(request);\n}\nfn execute_two(dispatcher: &Dispatcher, request: Request) {\n    dispatcher.dispatch(request);\n}\n" \
    "ARCH-SPRAWL"

assert_precommit_blocks "ARCH-SPRAWL: repeated dispatcher calls in separate hunks need arch-exempt plan" \
    "src/runtime.rs" \
    "fn existing() {}\n\n\n\n\n\n\n\n\n" \
    "fn existing() {}\nfn execute_one(dispatcher: &Dispatcher, request: Request) {\n    dispatcher.dispatch(request);\n}\n\n\n\n\n\n\n\n\nfn execute_two(dispatcher: &Dispatcher, request: Request) {\n    dispatcher.dispatch(request);\n}\n" \
    "ARCH-SPRAWL"

assert_precommit_allows "ARCH-SPRAWL: annotated repeated dispatcher calls are accepted" \
    "src/runtime.rs" \
    "fn existing() {}\n" \
    "// arch-exempt: parallel_dispatch, temporary split while gateway lands, plan #2800\nfn execute_one(dispatcher: &Dispatcher, request: Request) {\n    dispatcher.dispatch(request);\n}\nfn execute_two(dispatcher: &Dispatcher, request: Request) {\n    dispatcher.dispatch(request);\n}\n"

assert_precommit_allows "ARCH-SPRAWL: annotated repeated dispatcher calls in separate hunks are accepted" \
    "src/runtime.rs" \
    "fn existing() {}\n\n\n\n\n\n\n\n\n" \
    "fn existing() {}\n// arch-exempt: parallel_dispatch, temporary split while gateway lands, plan #2800\nfn execute_one(dispatcher: &Dispatcher, request: Request) {\n    dispatcher.dispatch(request);\n}\n\n\n\n\n\n\n\n\nfn execute_two(dispatcher: &Dispatcher, request: Request) {\n    dispatcher.dispatch(request);\n}\n"

# 覆盖:TQ TEST-SPEC-novel-studio-atomic-evolution §通用pre-commit §4 AE-L2-123/AE-L3-34
# Exercise the complete caller with real Git history/index and production gates.
# All writes (including synthetic refs and the hook) stay in a disposable repo.
assert_precommit_scope() {
    local scenario="$1" mode="$2" expected="$3"
    local tmp output status fixture_status base ancestor i lang
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/precommit-scope.XXXXXX")
    set +e
    (
        set -e
        cd "$tmp"
        git init -q -b main --template=
        export GIT_AUTHOR_NAME="Safety Test" GIT_COMMITTER_NAME="Safety Test"
        export GIT_AUTHOR_EMAIL="test@example.com" GIT_COMMITTER_EMAIL="test@example.com"
        mkdir -p .git/hooks scripts/ci src/channels/web/handlers src/cli \
            crates/ironclaw_gateway/static/i18n \
            crates/ironclaw_reborn_composition/src crates/other/src
        cp "$ROOT_DIR/scripts/pre-commit-safety.sh" scripts/
        cp "$ROOT_DIR/scripts/check-i18n-parity.sh" scripts/
        cp "$ROOT_DIR/scripts/ci/check-composition-budget.sh" scripts/ci/
        printf '%s\n' '[gate]' 'enforce = true' 'ceiling_bp = 1000' \
            'tolerance_bp = 0' 'arc_dyn_ceiling = 0' 'arc_dyn_tolerance = 0' \
            > scripts/ci/composition-budget.toml
        printf 'fn existing() {}\n' > src/runtime.rs
        printf 'fn handler() {}\n' > src/channels/web/handlers/demo.rs
        printf 'fn cli() {}\n' > src/cli/demo.rs
        printf 'fn existing() {}\n' > crates/ironclaw_reborn_composition/src/lib.rs
        for i in $(seq 1 20); do printf '// base %s\n' "$i"; done > crates/other/src/lib.rs
        for lang in en zh-CN ko; do
            printf "  'base': 'value',\n" > "crates/ironclaw_gateway/static/i18n/$lang.js"
        done
        git add .
        base=$(git -c commit.gpgSign=false commit-tree "$(git write-tree)" -m base)
        git update-ref refs/heads/main "$base"
        git checkout -qb feature
        git -c branch.feature.remote=. -c branch.feature.merge=refs/heads/main \
            rev-parse --verify '@{upstream}' >/dev/null

        case "$scenario" in
            dispatch-ancestor|credential-ancestor)
                if [ "$scenario" = dispatch-ancestor ]; then
                    printf 'fn handler() { state.store.create(); }\n' > src/channels/web/handlers/demo.rs
                else
                    printf 'fn handler() { let name: CredentialName; }\n' > src/channels/web/handlers/demo.rs
                fi
                git add src
                ancestor=$(git -c commit.gpgSign=false commit-tree "$(git write-tree)" -p "$base" -m ancestor)
                git update-ref refs/heads/feature "$ancestor"
                printf 'fn existing() {}\nfn changed() {}\n' > src/runtime.rs
                ;;
            dispatch-unstaged)
                printf 'fn handler() { state.store.create(); }\n' > src/channels/web/handlers/demo.rs
                printf 'fn existing() {}\nfn changed() {}\n' > src/runtime.rs
                ;;
            dispatch-staged|credential-staged)
                if [ "$scenario" = dispatch-staged ]; then
                    printf 'fn handler() { state.store.create(); }\n' > src/channels/web/handlers/demo.rs
                else
                    printf 'fn handler() { let name: CredentialName; }\n' > src/channels/web/handlers/demo.rs
                fi
                git add src/channels/web/handlers/demo.rs
                printf 'fn handler() {}\n' > src/channels/web/handlers/demo.rs
                ;;
            tx-standalone|tx-staged|tx-unstaged)
                printf 'fn existing() {\n    db.execute("one");\n    db.execute("two");\n}\n' > src/runtime.rs
                if [ "$scenario" = tx-staged ]; then
                    git add src/runtime.rs
                    printf 'fn existing() {}\n' > src/runtime.rs
                elif [ "$scenario" = tx-unstaged ]; then
                    printf '// staged comment\n' >> src/cli/demo.rs
                    git add src/cli/demo.rs
                fi
                ;;
            js-safe|js-breach)
                mkdir -p crates/ironclaw_gateway/static/js
                if [ "$scenario" = js-safe ]; then
                    printf 'const value = 1;\n' > crates/ironclaw_gateway/static/js/demo.js
                else
                    printf 'const value = ;\n' > crates/ironclaw_gateway/static/js/demo.js
                fi
                git add crates/ironclaw_gateway/static/js/demo.js
                if [ "$scenario" = js-safe ]; then
                    printf 'const value = ;\n' > crates/ironclaw_gateway/static/js/demo.js
                else
                    printf 'const value = 1;\n' > crates/ironclaw_gateway/static/js/demo.js
                fi
                ;;
            test-boundary)
                printf '// staged production\n\nfn existing() {\n    value.unwrap();\n}\n' > src/runtime.rs
                git add src/runtime.rs
                printf '#[cfg(test)]\nmod tests {\nfn existing() { value.unwrap(); }\n}\n' > src/runtime.rs
                ;;
            test-safe)
                printf '#[cfg(test)]\nmod tests {\nfn test_helper() { value.unwrap(); }\n}\n' > src/runtime.rs
                git add src/runtime.rs
                printf '// unstaged production\n\nfn existing() {\n    value.unwrap();\n}\n' > src/runtime.rs
                ;;
            pub-caller)
                printf 'fn existing() {}\npub fn unused() {}\n' > src/runtime.rs
                git add src/runtime.rs
                printf 'fn existing() { unused(); }\npub fn unused() {}\n' > src/runtime.rs
                ;;
            i18n-safe|i18n-breach)
                if [ "$scenario" = i18n-safe ]; then
                    printf '// staged comment\n' >> crates/ironclaw_gateway/static/i18n/en.js
                else
                    printf "  'extra': 'value',\n" >> crates/ironclaw_gateway/static/i18n/en.js
                fi
                git add crates/ironclaw_gateway/static/i18n/en.js
                if [ "$scenario" = i18n-safe ]; then
                    printf "  'extra': 'value',\n" >> crates/ironclaw_gateway/static/i18n/en.js
                else
                    printf "  'base': 'value',\n" > crates/ironclaw_gateway/static/i18n/en.js
                fi
                ;;
            composition-safe|composition-breach)
                if [ "$scenario" = composition-safe ]; then
                    printf '// staged comment\n' >> crates/other/src/lib.rs
                    git add crates/other/src/lib.rs
                else
                    for i in $(seq 1 4); do printf '// staged growth %s\n' "$i"; done > crates/ironclaw_reborn_composition/src/lib.rs
                    git add crates/ironclaw_reborn_composition/src/lib.rs
                fi
                if [ "$scenario" = composition-safe ]; then
                    for i in $(seq 1 4); do printf '// unstaged growth %s\n' "$i"; done > crates/ironclaw_reborn_composition/src/lib.rs
                else
                    printf 'fn existing() {}\n' > crates/ironclaw_reborn_composition/src/lib.rs
                fi
                ;;
            git-error)
                printf 'corrupted fixture index\n' > .git/index
                ;;
            non-rust)
                printf 'fn handler() { state.store.create(); }\n' > src/channels/web/handlers/demo.rs
                printf 'fn cli() { let name: CredentialName; }\n' > src/cli/demo.rs
                printf 'safe staged text\n' > change.txt
                git add change.txt
                ;;
            *) exit 2 ;;
        esac
        case "$scenario" in
            dispatch-ancestor|credential-ancestor|dispatch-unstaged)
                if [ "$mode" != standalone ]; then git add src/runtime.rs; fi ;;
        esac
        if [ "$mode" = hook ]; then
            cp scripts/pre-commit-safety.sh .git/hooks/pre-commit
            chmod +x .git/hooks/pre-commit
            if output=$(git -c core.hooksPath=.git/hooks \
                -c branch.feature.remote=. -c branch.feature.merge=refs/heads/main \
                hook run pre-commit 2>&1); then status=0; else status=$?; fi
        else
            if output=$(git -c branch.feature.remote=. -c branch.feature.merge=refs/heads/main \
                -c core.hooksPath=.git/hooks -c alias.safety='!bash scripts/pre-commit-safety.sh' \
                safety 2>&1); then status=0; else status=$?; fi
        fi
        if { [ "$expected" = allow ] && [ "$status" -eq 0 ]; } || \
            { [ "$expected" != allow ] && [ "$status" -ne 0 ] && \
                printf '%s\n' "$output" | grep -Fq "$expected"; }; then
            printf 'OK: scope %s / %s (status %s)\n' "$scenario" "$mode" "$status"
            exit 0
        fi
        printf 'FAIL: scope %s / %s expected %s, got status %s\n%s\n' \
            "$scenario" "$mode" "$expected" "$status" "$output"
        exit 1
    )
    fixture_status=$?
    set -e
    rm -rf "$tmp"
    if [ "$fixture_status" -eq 0 ]; then PASS=$((PASS + 1)); else FAIL=$((FAIL + 1)); fi
}

assert_precommit_scope dispatch-ancestor staged allow
assert_precommit_scope credential-ancestor staged allow
assert_precommit_scope dispatch-unstaged staged allow
assert_precommit_scope non-rust staged allow
assert_precommit_scope dispatch-staged staged DISPATCH
assert_precommit_scope credential-staged staged CREDNAME
assert_precommit_scope dispatch-ancestor standalone DISPATCH
assert_precommit_scope credential-ancestor standalone CREDNAME
assert_precommit_scope tx-standalone standalone TX
assert_precommit_scope tx-staged staged TX
assert_precommit_scope tx-unstaged staged allow
assert_precommit_scope js-safe staged allow
assert_precommit_scope js-breach staged 'syntax error'
assert_precommit_scope test-boundary staged PANIC
assert_precommit_scope test-safe staged allow
assert_precommit_scope pub-caller staged PUBAPI
assert_precommit_scope i18n-safe staged allow
assert_precommit_scope i18n-breach staged 'i18n parity check failed'
assert_precommit_scope composition-safe staged allow
assert_precommit_scope composition-breach staged 'composition mass budget exceeded'
assert_precommit_scope dispatch-ancestor hook allow
assert_precommit_scope dispatch-staged hook DISPATCH
assert_precommit_scope git-error staged fatal

# 覆盖:同SSOT PCS-TEST-CFG/PCS-PROD-CFG，真实命名测试模块形态与生产可达反例。
assert_precommit_allows "PANIC: actual mount_view_tests all(test, any(features)) is test-only" \
    "src/runtime.rs" "fn existing() {}\n" \
    'fn existing() {}\n#[cfg(all(test, any(feature = "libsql", feature = "postgres")))]\nmod mount_view_tests {\n    fn mount_view() { value.unwrap(); assert!(true); }\n}\n'
assert_precommit_allows "PANIC: named cfg(test) module is test-only" \
    "src/runtime.rs" "fn existing() {}\n" \
    'fn existing() {}\n#[cfg(test)]\nmod mount_view_tests {\n    fn mount_view() { value.unwrap(); }\n}\n'
assert_precommit_allows "PANIC: multiline all(test, features) is test-only" \
    "src/runtime.rs" "fn existing() {}\n" \
    'fn existing() {}\n#[cfg(all(\n    test,\n    any(feature = "libsql", feature = "postgres")\n))]\nmod mount_view_tests {\n    fn mount_view() { value.unwrap(); }\n}\n'
assert_precommit_blocks "PANIC: same named module without cfg remains production" \
    "src/runtime.rs" "fn existing() {}\n" \
    'fn existing() {}\nmod mount_view_tests {\n    fn mount_view() { value.unwrap(); }\n}\n' PANIC
assert_precommit_blocks "PANIC: any(test, feature) can compile in production" \
    "src/runtime.rs" "fn existing() {}\n" \
    'fn existing() {}\n#[cfg(any(test, feature = "libsql"))]\nmod mount_view_tests {\n    fn mount_view() { value.unwrap(); }\n}\n' PANIC
assert_precommit_blocks "PANIC: production after closed test module still blocks" \
    "src/runtime.rs" "fn existing() {}\n" \
    'fn existing() {}\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n}\nfn production() { value.unwrap(); }\n' PANIC
assert_precommit_blocks "PANIC: production mod tests hunk context cannot exempt" \
    "src/runtime.rs" 'mod tests {\n    fn existing() {}\n}\n' \
    'mod tests {\n    fn existing() {}\n    fn production() { value.unwrap(); }\n}\n' PANIC

echo ""
echo "Passed: $PASS, Failed: $FAIL"
[ "$FAIL" -eq 0 ]
