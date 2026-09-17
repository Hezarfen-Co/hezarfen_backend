#!/usr/bin/env bash
# The LOCAL full gate: the "special path" where this machine proves the suite
# instead of a GitHub runner.
#
# Why it exists: the same suite is 565s here and ~55min on a 4-core runner.
# When you have run it, commit the `Local-Gate:` trailer this script prints
# (`--amend` writes it for you) and the push carries proof: the workflow's
# "Decide test gate" step verifies the trailer against HEAD^{tree} and then
# skips the runner suite, so CI only packs and deploys.
#
# Default path (nothing here): push without a trailer and GitHub runs
# everything, suite included. Nothing is mandatory; this is the fast lane.
#
# What it runs, in order, each fatal on failure:
#   1. cargo clippy --all-targets -- -D warnings      (same lint CI runs)
#   2. cargo nextest run --no-fail-fast               (the whole suite, or
#      the --scope filter; CI's env, and DATABASE_URL unset so the harness
#      picks its own per-test databases)
#   3. cargo test --test doc_sync                     (README drift gate)
#
# On success it writes .git/local-gate/<tree-sha>.ok (inside .git, so it is
# never committed) holding the tree, the timestamp, the scope, the exact
# commands and the counts parsed from nextest's summary line. Re-running for
# the same tree is a no-op that just re-prints the trailer — the stamp IS the
# proof, so `--force` is the only way to pay for the suite twice.
#
# The trailer is printed only for a FULL run: a --scope run is quick
# iteration, not a claim about the suite.
#
# Prerequisites: cargo-nextest, and a reachable Postgres (the suite mints a
# control+school database pair per test). Both are checked first and reported
# with the exact command to fix them.
#
# Usage:
#   scripts/gate-local.sh                 # full gate (reuses a stamp if it exists)
#   scripts/gate-local.sh --force         # full gate, even if already stamped
#   scripts/gate-local.sh --scope e2e     # quick iteration (no trailer)
#   scripts/gate-local.sh --amend         # full gate, then append the
#                                         # trailer to the HEAD commit
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$here"

scope=""
amend=0
force=0
while [ $# -gt 0 ]; do
    case "$1" in
        --scope)
            [ $# -ge 2 ] || { echo "--scope needs a nextest filter (e.g. --scope e2e)" >&2; exit 2; }
            scope=$2
            shift 2
            ;;
        --amend)
            amend=1
            shift
            ;;
        --force)
            force=1
            shift
            ;;
        -h|--help)
            sed -n '/^# Usage:/,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown argument: $1 (see --help)" >&2
            exit 2
            ;;
    esac
done

# ── Record what this run is ─────────────────────────────────────────────────
tree=$(git rev-parse HEAD^{tree})
head=$(git rev-parse --short HEAD)
stamp_dir=$(git rev-parse --git-dir)/local-gate
mkdir -p "$stamp_dir"
stamp=$stamp_dir/$tree.ok
log=$stamp_dir/$tree.log

if [ -n "$scope" ]; then
    scope_label=$scope
else
    scope_label=full
fi

dirty=no
if [ -n "$(git status --porcelain)" ]; then
    dirty=yes
    cat <<'EOF'
WARNING: the working tree is dirty. The gate will test what is on disk, but a
         stamp can only describe a committed tree — so NO stamp and no trailer
         will be written this run. Commit, then re-run to get one.
EOF
fi

# The tail both paths share: print (and optionally amend in) the trailer.
emit_trailer() {
    if [ "$scope_label" != full ]; then
        cat <<EOF

== scoped run ($scope_label): no Local-Gate trailer.
   The trailer is a claim about the FULL suite, so only an unscoped run
   produces it. Re-run \`scripts/gate-local.sh\` before pushing if you want
   GitHub to skip its own suite.
EOF
        return 0
    fi
    local trailer="Local-Gate: $tree (${passed:-0} passed, ${seconds:-0}s)"
    if [ "$amend" = 1 ]; then
        if git branch -r --contains HEAD 2>/dev/null | grep -q .; then
            echo "REFUSING --amend: HEAD is already on a remote branch; amend would rewrite published history." >&2
            echo "Add the trailer by hand instead:" >&2
            echo "    $trailer" >&2
            exit 1
        fi
        local message
        message=$(git log -1 --format=%B)
        if printf '%s\n' "$message" | grep -q '^Local-Gate: '; then
            message=$(printf '%s\n' "$message" | sed "s|^Local-Gate: .*|$trailer|")
        else
            message=$(printf '%s\n' "$message" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}')
            message="$message
$trailer"
        fi
        printf '%s\n' "$message" | git commit --amend -q --no-verify -F -
        echo "== appended to $(git rev-parse --short HEAD) (tree unchanged: $(git rev-parse HEAD^{tree}))"
    fi
    cat <<EOF

Add this line to the commit you are about to push:

    $trailer

(\`scripts/gate-local.sh --amend\` appends it to HEAD for you — the tree does
not change, so an amend cannot invalidate the stamp.) The push then carries a
verified claim: the workflow's "Decide test gate" step compares the trailer's
tree with HEAD^{tree} and skips the runner suite only when they match.
EOF
}

# Already proven on this exact tree: the stamp IS the evidence, so re-running
# the suite would only burn ten minutes to arrive at the same line.
if [ "$force" = 0 ] && [ "$dirty" = no ] && [ -f "$stamp" ]; then
    scope_label=$(sed -n 's/^scope: //p' "$stamp" | tail -1)
    passed=$(sed -n 's/^passed: //p' "$stamp" | tail -1)
    seconds=$(sed -n 's/^duration_seconds: //p' "$stamp" | tail -1)
    echo "== tree $tree already verified on this machine; reusing $stamp (--force to re-run)"
    cat "$stamp"
    emit_trailer
    exit 0
fi

# ── Prerequisites ───────────────────────────────────────────────────────────
# Each says what is missing and the exact command that fixes it: a mysterious
# failure 90 seconds in is worse than not starting.
command -v cargo-nextest >/dev/null || {
    cat >&2 <<'EOF'
cargo-nextest is not installed — the suite runs under nextest, not `cargo test`.
    cargo install cargo-nextest --locked
EOF
    exit 1
}

# The compose stack's Postgres first (that is the repo's own container, and
# the one with max_connections=500 the parallel suite needs), then the CI
# sidecar's name in case it was started locally, then a plain reachable
# server on the harness's default address.
pg_container=""
pg_tool=""
for candidate in hezarfen_backend_postgres hezarfen_backend_pg; do
    for tool in podman docker; do
        if command -v "$tool" >/dev/null &&
            "$tool" ps --format '{{.Names}}' 2>/dev/null | grep -qx "$candidate"; then
            pg_container=$candidate
            pg_tool=$tool
            break 2
        fi
    done
done

if [ -n "$pg_container" ]; then
    "$pg_tool" exec "$pg_container" pg_isready -U hezarfen >/dev/null 2>&1 || {
        echo "Postgres container $pg_container exists but is not answering yet; wait for it (compose healthcheck) and re-run." >&2
        exit 1
    }
    max_conn=$("$pg_tool" exec "$pg_container" psql -U hezarfen -tAc 'show max_connections' 2>/dev/null || echo '?')
    echo "== Postgres: container $pg_container ($pg_tool), max_connections=$max_conn"
    if [ "$max_conn" != '?' ] && [ "$max_conn" -lt 500 ] 2>/dev/null; then
        echo "   WARNING: max_connections=$max_conn; the suite mints roughly a dozen control+school database pairs concurrently and the compose stack raises this to 500. Expect connection errors."
    fi
elif (exec 3<>/dev/tcp/127.0.0.1/5432) 2>/dev/null; then
    echo "== Postgres: server answering on 127.0.0.1:5432 (no repo container; connection ceiling not verifiable)"
else
    cat >&2 <<'EOF'
No Postgres for the test harness: no hezarfen_backend_postgres / hezarfen_backend_pg
container is running and nothing answers on 127.0.0.1:5432.

The suite mints a control+school database pair per test, so it needs the repo's
own server. Start it with:

    podman compose up -d postgres

(that is the container named hezarfen_backend_postgres, published on
127.0.0.1:5432 with max_connections=500 — see compose.yaml).
EOF
    exit 1
fi

: > "$log"
echo "== tree $tree (HEAD $head), scope: $scope_label, log: $log"

# ── The three steps ─────────────────────────────────────────────────────────
declare -a commands=()

run_step() {
    local label=$1
    shift
    commands+=("$(printf '%q ' "$@")")
    echo
    echo "=== $label"
    echo "+ $(printf '%q ' "$@")"
    if "$@" 2>&1 | tee -a "$log"; then
        echo "--- $label: OK"
    else
        local rc=$?
        echo "--- $label: FAILED (exit $rc) — nothing was stamped; fix it and re-run" >&2
        exit $rc
    fi
}

# CI runs the same lint on its own runner before anything is packaged; a
# warning that only shows up there costs a full round trip.
run_step "clippy (same as CI's Clippy job)" \
    cargo clippy --all-targets -- -D warnings

# CI's env, verbatim: no debug info in the test/dev profiles (the binaries
# blow past a runner's disk), no incremental artifacts, sqlx offline cache.
# DATABASE_URL is unset on purpose — the harness mints its own databases and a
# stray value would point every test at one schema.
if [ -n "$scope" ]; then
    run_step "nextest (scope: $scope)" \
        env -u DATABASE_URL \
        CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 \
        SQLX_OFFLINE=true \
        cargo nextest run --no-fail-fast --config-file .config/nextest.toml "$scope"
else
    run_step "nextest (full suite)" \
        env -u DATABASE_URL \
        CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 \
        SQLX_OFFLINE=true \
        cargo nextest run --no-fail-fast --config-file .config/nextest.toml
fi

# The README's ## Endpoints table is generated; this is the drift gate. No
# UPDATE_DOCS: a run that regenerates what it should be checking is not a gate.
run_step "doc gate (README drift)" \
    cargo test --test doc_sync

# ── Parse nextest's summary line ────────────────────────────────────────────
summary=$(grep -E 'Summary \[.*tests run:' "$log" | tail -1 || true)
if [ -z "$summary" ]; then
    echo "no nextest summary line in $log — the counts are unknown, so no stamp is written" >&2
    exit 1
fi
# Every parse is `|| true`: "0 failed" / "0 skipped" simply do not appear in
# the summary line, and a bare `grep -oE` with no match exits 1 — which, under
# `set -e` + pipefail, would abort a run that passed.
duration=$(printf '%s\n' "$summary" | grep -oE '\[ *[0-9][0-9.]*s\]' | grep -oE '[0-9][0-9.]*' | head -1 || true)
total=$(printf '%s\n' "$summary" | grep -oE '[0-9]+ tests run' | grep -oE '[0-9]+' | head -1 || true)
passed=$(printf '%s\n' "$summary" | grep -oE '[0-9]+ passed' | grep -oE '[0-9]+' | head -1 || true)
skipped=$(printf '%s\n' "$summary" | grep -oE '[0-9]+ skipped' | grep -oE '[0-9]+' | head -1 || true)
failed=$(printf '%s\n' "$summary" | grep -oE '[0-9]+ failed' | grep -oE '[0-9]+' | head -1 || true)
if [ -z "$total" ] || [ -z "$passed" ]; then
    echo "could not read the counts out of: $summary" >&2
    exit 1
fi
seconds=${duration%%.*}
counts="$total tests run, $passed passed, ${failed:-0} failed, ${skipped:-0} skipped"

echo
echo "== $summary"
echo "== counts: $counts (${seconds}s)"

if [ "$dirty" = yes ]; then
    echo "== no stamp written (dirty working tree); commit and re-run"
    exit 0
fi

# ── The stamp ───────────────────────────────────────────────────────────────
{
    echo "tree: $tree"
    echo "head: $head"
    echo "time: $(date -Is)"
    echo "scope: $scope_label"
    echo "dirty: $dirty"
    echo "counts: $counts"
    echo "passed: ${passed:-0}"
    echo "duration_seconds: ${seconds:-0}"
    for command in "${commands[@]}"; do
        echo "command: ${command% }"
    done
} > "$stamp"

echo "== stamp: $stamp"
cat "$stamp"

emit_trailer
