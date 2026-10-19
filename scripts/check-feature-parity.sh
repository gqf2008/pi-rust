#!/bin/sh
# Fail when AGENTS.md's opt-in feature list drifts from CI's.
#
# AGENTS.md tells contributors (and agents) to run the opt-in plugin features as
# part of the local gate, and warns in the same breath that skipping them lets the
# features rot. That instruction is only true if the list matches what CI runs: a
# list that is one short makes the local gate *weaker* than CI, so everything
# looks green here while CI is red — on exactly the plugin nobody exercised.
#
# That happened: plugin-kiqi-codegraph and plugin-kiqi-orchestrator were missing
# from AGENTS.md while CI ran them.
#
# The two lists are compared as sets, so ordering and line wrapping are free.
#
# POSIX sh only: no process substitution (`<(...)`), no `grep -f /dev/stdin`.
# The first version used both and silently reported "agree" even after a feature
# was removed — a check that cannot fail is worse than no check.
set -eu

CI_FILE=".github/workflows/ci.yml"
DOC_FILE="AGENTS.md"

[ -f "$CI_FILE" ] || { echo "missing $CI_FILE" >&2; exit 1; }
[ -f "$DOC_FILE" ] || { echo "missing $DOC_FILE" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Every `plugin-kiqi-*` token mentioned in each file, one per line, deduped.
# Only lines that actually pass `--features` count. Scanning the whole document
# would let prose mask a gap: this file's own explanatory comment names the two
# features that were missing, so a document-wide scan reported "agree" even after
# they were deleted from the command.
#
# `grep -o -w` with a word-character class: a class that also contains `-` makes
# grep split at the hyphen and can emit a bare `plugin-kiqi-` token, and an empty
# pattern line silently disables every `-f` lookup below.
features_line() {
    grep -- '--features' "$1" | grep -o -w -E 'plugin-kiqi-[[:alnum:]_]+' | sort -u
}

features_line "$CI_FILE" > "$tmp/ci"
features_line "$DOC_FILE" > "$tmp/doc"

# In CI but not in the doc: the doc's gate is looser than CI's. Hard error.
grep -v -x -F -f "$tmp/doc" "$tmp/ci" > "$tmp/missing" || true
# In the doc but not in CI: worth a note, not a failure (prose may name one).
grep -v -x -F -f "$tmp/ci" "$tmp/doc" > "$tmp/extra" || true

status=0

if [ -s "$tmp/missing" ]; then
    echo "AGENTS.md is missing feature(s) that CI runs:" >&2
    sed 's/^/  /' "$tmp/missing" >&2
    status=1
fi

if [ -s "$tmp/extra" ]; then
    echo "note: AGENTS.md names feature(s) CI does not list:" >&2
    sed 's/^/  /' "$tmp/extra" >&2
fi

if [ "$status" -eq 0 ]; then
    count=$(wc -l < "$tmp/ci" | tr -d ' ')
    echo "opt-in feature lists agree ($count features)"
fi

exit "$status"
