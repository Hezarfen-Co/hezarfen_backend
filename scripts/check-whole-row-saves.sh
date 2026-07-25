#!/bin/sh
# Ban whole-row saves: `db.update(...).content(self)` and friends.
#
# WHY: `.content(self)` REPLACES the entire stored row with the in-memory
# struct. If `self` was loaded before an `.await` (a permission check, another
# query, an HTTP/QUIC call), any concurrent PATCH that landed in between is
# silently reverted — the write succeeds, the other user's change is gone, no
# error anywhere. A grep cannot see "read before an await", so the shape itself
# is banned and the exceptions are made explicit and reviewable.
#
# FIX: write only the fields the request actually changed, e.g.
#     db.query("UPDATE $id SET text = $text, points = $points")
# or hold a lock across the whole read-modify-write and opt out below.
#
# OPT OUT: put this on the line IMMEDIATELY above the call:
#     // whole-row-save-ok: <why this row cannot be raced>
#
# Usage:
#   scripts/check-whole-row-saves.sh            # whole tree (gate mode)
#   scripts/check-whole-row-saves.sh a.rs b.rs  # only these files (hook mode)
# Exit 1 if any unannotated whole-row save is found.

cd "$(dirname "$0")/.." || exit 1

if [ "$#" -gt 0 ]; then
    set -- "$@"
else
    # shellcheck disable=SC2046  # paths in this repo have no spaces
    set -- $(find src -name '*.rs' | sort)
fi
[ "$#" -eq 0 ] && exit 0

awk '
FNR == 1 { prev = "" }
/\.content\(&?self[).]/ {
    if (prev !~ /\/\/[ \t]*whole-row-save-ok:[ \t]*[^ \t]/) {
        sub(/^[ \t]+/, "", $0)
        printf "%s:%d: %s\n", FILENAME, FNR, $0
        bad++
    }
}
{ prev = $0 }
END {
    if (bad) {
        print ""
        print "whole-row save: .content(self) replaces the ENTIRE row, so any"
        print "concurrent PATCH that landed after this struct was read (i.e."
        print "across any .await above) is silently reverted — no error, data"
        print "just disappears. Write only the changed fields instead:"
        print "    db.query(\"UPDATE $id SET field = $field\")"
        print "If the row genuinely cannot be raced (e.g. a write lock is held"
        print "across the read and the write), opt out on the line directly above:"
        print "    // whole-row-save-ok: <why this row cannot be raced>"
        print ""
        printf "%d unannotated whole-row save(s).\n", bad
        exit 1
    }
}
' "$@"
