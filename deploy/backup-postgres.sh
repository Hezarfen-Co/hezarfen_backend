#!/usr/bin/env bash
# Dump every database in the hezarfen compose stack to backups/ next to this
# script. pg_dumpall is required (not pg_dump): each school is its own
# postgres database, and a control-only dump would lose them all.
#
# Keeps: backups/hezarfen-<UTC>.sql.gz, mode 0600.
# Cron example (daily 04:20):
#   20 4 * * * $HOME/hezarfen_backend/backup-postgres.sh
#
# Restore (from ~/hezarfen_backend):
#   gunzip -c backups/FILE.sql.gz | podman exec -i hezarfen-postgres psql -U hezarfen -d postgres
set -euo pipefail

dir="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$dir/backups"

# POSTGRES_USER lives in the env file beside this script (plain KEY=value
# lines, no quotes/export); default hezarfen.
user="$(awk -F= '/^POSTGRES_USER=/{print $2; exit}' "$dir/hezarfen_backend.env" 2>/dev/null || true)"
user="${user:-hezarfen}"

out="$dir/backups/hezarfen-$(date -u +%Y%m%dT%H%M%SZ).sql.gz"
umask 077
# pipefail (set above): a failed pg_dumpall must not leave a "good" empty dump.
podman exec hezarfen-postgres pg_dumpall -U "$user" | gzip > "$out"
echo "wrote $out ($(du -h "$out" | cut -f1))"
