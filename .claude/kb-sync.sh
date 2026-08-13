#!/usr/bin/env bash
# Keep README.md and the Claude SurrealDB kb in sync.
#   kb-sync.sh session   SessionStart: re-ingest kb if README drifted, warn on
#                        src modules missing from README
#   kb-sync.sh post      PostToolUse (Edit|Write): README edited -> re-ingest;
#                        src/*.rs edited -> warn if module missing from README
cd "$(dirname "$0")/.." || exit 0
SURREAL="$HOME/.surrealdb/surreal"
DB="surrealkv://$HOME/.claude/kb/kb.db"
kb() { "$SURREAL" sql -e "$DB" --ns claude --db hezarfen_backend --json --hide-welcome --log error; }

ingest() {
  have=$(echo 'SELECT VALUE sha256 FROM meta:readme;' | kb 2>/dev/null | grep -o '[0-9a-f]\{64\}')
  want=$(sha256sum README.md | cut -d' ' -f1)
  if [ "$have" != "$want" ]; then
    python3 "$HOME/.claude/kb/ingest.py" README.md | kb >/dev/null 2>&1
    python3 "$HOME/.claude/kb/ingest-endpoints.py" README.md | kb >/dev/null 2>&1
  fi
}

check() {
  # $2 = the single edited src file (post); empty = full sweep (session)
  # corner-cut: substring match on "<name>.rs" anywhere in README — a module
  # mentioned outside ## Layout also counts; tighten to the section if it lies
  mods="${2:-src/domain/*.rs src/web/*.rs}"
  # only domain/web files are ## Layout modules (lib.rs, config.rs, ... are not)
  case "$mods" in *src/domain/*.rs|*src/web/*.rs) ;; *) mods="" ;; esac
  missing=""
  for f in $mods; do
    b=$(basename "$f" .rs)
    [ "$b" = mod ] && continue
    grep -q "$b\.rs" README.md || missing="$missing $b"
  done
  # Route drift: every utoipa path under a lib.rs nest prefix must appear
  # backticked in the README ## Endpoints table. Only web/ and lib.rs edits can
  # introduce drift, so a domain-file edit skips this sweep.
  routes=""
  case "${2:-src/lib.rs}" in
    *src/web/*.rs|*src/lib.rs)
      while read -r prefix mod; do
        while read -r p; do
          full="$prefix$p"; full="${full%/}"
          grep -qF "\`$full\`" README.md || routes="$routes $full"
        done < <(grep -oP 'path = "\K[^"]*' "src/web/$mod.rs" 2>/dev/null)
      done < <(grep -oP '\.nest\("\K[^"]+", web::\w+' src/lib.rs | sed 's/", web::/ /')
      ;;
  esac
  msg=""
  [ -n "$missing" ] && msg="README.md ## Layout is missing src modules:$missing."
  [ -n "$routes" ] && msg="$msg README.md ## Endpoints is missing routes:$routes."
  if [ -n "$msg" ]; then
    printf '{"hookSpecificOutput":{"hookEventName":"%s","additionalContext":"%s Update README before finishing. kb re-ingest is automatic."}}\n' "$1" "$msg"
  fi
}

case "$1" in
  session)
    ingest
    check SessionStart
    ;;
  post)
    fp=$(jq -r '.tool_input.file_path // empty' 2>/dev/null)
    case "$fp" in
      */README.md) ingest ;;
      */src/*.rs) check PostToolUse "$fp" ;;
    esac
    ;;
esac
exit 0
