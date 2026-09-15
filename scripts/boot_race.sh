#!/usr/bin/env bash
# Boot N copies of the backend at once against a Postgres whose school
# template has just been dropped, and report what each boot did.
#
# This is the reproduction for the concurrent-boot class. Two things are being
# watched:
#   * every boot must reach `listening` — overlapping `CREATE DATABASE`s used
#     to kill the losers, because Postgres reports the losing side as a
#     `pg_database_datname_index` unique violation (`23505`) rather than the
#     `42P04` duplicate-database error, and the swallow only matched `42P04`;
#   * the server log must stay clean *of create refusals* — a create that is
#     issued and refused is logged by the server even when the client swallows
#     the error, which is why the create now runs behind a probe and under an
#     advisory lock. The assertion is scoped to those lines so that a test
#     suite sharing the server (its deliberate foreign-key violations) does
#     not read as a boot failure.
# Exit status is non-zero if any boot failed or the server logged a create
# refusal.
#
# The template of the control database is dropped first; the next boot (this
# script, or your usual `podman compose up -d backend`) rebuilds and migrates
# it, so a dev database is the only thing this may disturb.
#
# Needs the compose Postgres up (`podman compose up -d postgres`) and a built
# binary (`cargo build`).
set -euo pipefail

CONTAINER="${HEZARFEN_PG_CONTAINER:-hezarfen_backend_postgres}"
CONTROL_DB="${HEZARFEN_CONTROL_DB:-hezarfen_control}"
N="${N:-8}"
BASE_PORT="${BASE_PORT:-7800}"
OUT="${OUT:-$(mktemp -d)}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$HERE/target/debug/hezarfen_backend}"

psql() { podman exec -i "$CONTAINER" psql -U hezarfen -v ON_ERROR_STOP=1 "$@"; }

echo "== dropping ${CONTROL_DB}_school_template (the next boot rebuilds it)"
psql -d postgres -q -c "DROP DATABASE IF EXISTS \"${CONTROL_DB}_school_template\""
before=$(podman logs "$CONTAINER" 2>&1 | wc -l)

echo "== racing $N boots on ports $BASE_PORT..$((BASE_PORT + N - 1))"
for i in $(seq 0 $((N - 1))); do
    (
        HOST=127.0.0.1 PORT=$((BASE_PORT + i)) \
            DATABASE_URL="postgres://hezarfen:hezarfen@127.0.0.1:5432/${CONTROL_DB}" \
            FILES_PATH="${FILES_PATH:-/tmp/hez-files}" RUST_LOG=info \
            timeout 30 "$BIN" >"$OUT/boot$i.log" 2>&1
    ) &
done
wait

fail=0
for i in $(seq 0 $((N - 1))); do
    if grep -q "listening on" "$OUT/boot$i.log"; then
        echo "  boot$i (port $((BASE_PORT + i))): up"
    else
        echo "  boot$i (port $((BASE_PORT + i))): FAILED"
        tail -3 "$OUT/boot$i.log"
        fail=1
    fi
done

errors=$(podman logs "$CONTAINER" 2>&1 | tail -n +$((before + 1)) |
    grep -E 'pg_database_datname_index|already exists' | grep -cE 'ERROR|FATAL' || true)
echo "== server log since the race: $errors create-refusal line(s)"
[ "$errors" -eq 0 ] || fail=1

echo "== templates now on the server"
psql -d postgres -Atc "SELECT datname FROM pg_database WHERE datname LIKE '%_school_template'"
echo "== boot logs: $OUT"
exit "$fail"
