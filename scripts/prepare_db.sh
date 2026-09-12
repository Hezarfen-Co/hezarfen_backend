#!/usr/bin/env bash
# Refresh the sqlx "prepare" database: the ONE database whose schema is the
# union of migrations/control/*.sql + migrations/school/*.sql, against which
# sqlx's compile-time query macros check every query (`cargo sqlx prepare`,
# and plain `cargo check` with DATABASE_URL set — see .env.example).
#
# DISJOINTNESS INVARIANT: control tables (school, builder, builder_session,
# rate_limit) and school tables must never collide on a name — the union is
# applied to this single database. The loop below fails loudly (psql
# ON_ERROR_STOP=1) if a name ever collides.
#
# Raw apply, on purpose: no `sqlx migrate run`, so no `_sqlx_migrations`
# tracking rows exist and the control and school file sets never share a
# tracking table. The database is dropped and recreated on every run, so the
# result is deterministic.
#
# Requires the compose Postgres: `podman compose up -d postgres`.
set -euo pipefail

CONTAINER="${HEZARFEN_PG_CONTAINER:-hezarfen-postgres}"
PREPARE_DB="${HEZARFEN_PREPARE_DB:-hezarfen_sqlx_prepare}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

psql() {
    podman exec -i "$CONTAINER" psql -U hezarfen -v ON_ERROR_STOP=1 "$@"
}

echo "== refreshing $PREPARE_DB on $CONTAINER"
psql -d postgres -q <<SQL
DROP DATABASE IF EXISTS $PREPARE_DB;
CREATE DATABASE $PREPARE_DB;
SQL

applied=0
for dir in control school; do
    for file in "$HERE"/migrations/"$dir"/*.sql; do
        echo "== applying migrations/$dir/$(basename "$file")"
        psql -d "$PREPARE_DB" -q -f - < "$file"
        applied=$((applied + 1))
    done
done

echo "== applied $applied migration files; tables now in $PREPARE_DB:"
psql -d "$PREPARE_DB" -c '\dt'
