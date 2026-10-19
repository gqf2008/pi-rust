#!/bin/sh
# Fail when AGENTS.md's local gate drifts from what CI actually runs.
#
# AGENTS.md tells contributors (and agents) to run the opt-in plugin features and
# to build each plugin standalone, and warns that skipping the first lets the
# features rot. Both instructions are only true if the lists match CI's: a list
# that is one short makes the local gate *weaker* than CI, so everything looks
# green here while CI is red — on exactly the plugin nobody exercised.
#
# That happened once already (plugin-kiqi-codegraph and plugin-kiqi-orchestrator
# were missing), which is why this checks two things now:
#
#   1. the `--features plugin-kiqi-*` list  → `cargo test -p kiqi-cli`
#   2. the `-p kiqi-*` list of the standalone cdylib build
#
# The two CI lists are also compared with each other: every plugin with a wasm port
# is opt-in and every opt-in plugin has a cdylib entrypoint, so a plugin that gets
# added to one list but not the other is a forgotten registration, not a choice.
#
# POSIX sh only: no process substitution (`<(...)`), no `grep -f /dev/stdin`.
# Earlier versions used both plus a `-`-containing character class, and each one
# made the check silently pass while checking nothing.
set -eu

CI_FILE=".github/workflows/ci.yml"
DOC_FILE="AGENTS.md"

[ -f "$CI_FILE" ] || { echo "missing $CI_FILE" >&2; exit 1; }
[ -f "$DOC_FILE" ] || { echo "missing $DOC_FILE" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The feature names on the line that passes `--features` to cargo, as bare plugin
# names (`plugin-kiqi-goal` -> `kiqi-goal`).
feature_names() {
    # The trailing character must not be a hyphen, and hyphens are allowed inside:
    # `plugin-kiqi-ask-user` must survive whole. (A class ending in `-` used to make
    # grep emit a bare `plugin-kiqi-` token.)
    grep -- '--features' "$1" \
        | grep -o -E 'plugin-kiqi-[[:alnum:]_][[:alnum:]_-]*[[:alnum:]_]' \
        | sed 's/^plugin-//' \
        | sort -u
}

# The crate names on the line that builds the plugins standalone.
standalone_names() {
    # CI wraps the command over several lines with a trailing `\`, the document
    # keeps it on one; collect the crates from either shape by scanning from the
    # `cargo build --locked` line to the next blank line.
    awk '/cargo build --locked/{flag=1} flag{print} flag && /^[[:space:]]*$/{exit}' "$1" \
        | grep -o -E 'kiqi-[[:alnum:]_][[:alnum:]_-]*[[:alnum:]_]' \
        | grep -v '^kiqi-cli$' \
        | sort -u
}

feature_names "$CI_FILE" > "$tmp/ci_features"
feature_names "$DOC_FILE" > "$tmp/doc_features"
standalone_names "$CI_FILE" > "$tmp/ci_standalone"
standalone_names "$DOC_FILE" > "$tmp/doc_standalone"

status=0

report_missing() {
    # $1 label, $2 file with the missing entries
    if [ -s "$2" ]; then
        echo "AGENTS.md is missing from the $1 list:" >&2
        sed 's/^/  /' "$2" >&2
        status=1
    fi
}

grep -v -x -F -f "$tmp/doc_features" "$tmp/ci_features" > "$tmp/missing_features" || true
grep -v -x -F -f "$tmp/doc_standalone" "$tmp/ci_standalone" > "$tmp/missing_standalone" || true

report_missing "opt-in feature" "$tmp/missing_features"
report_missing "standalone cdylib build" "$tmp/missing_standalone"

# A plugin in one CI list but not the other is a forgotten registration.
grep -v -x -F -f "$tmp/ci_standalone" "$tmp/ci_features" > "$tmp/ci_only_features" || true
grep -v -x -F -f "$tmp/ci_features" "$tmp/ci_standalone" > "$tmp/ci_only_standalone" || true
if [ -s "$tmp/ci_only_features" ]; then
    echo "CI tests these as opt-in features but does not build them standalone:" >&2
    sed 's/^/  /' "$tmp/ci_only_features" >&2
    status=1
fi
if [ -s "$tmp/ci_only_standalone" ]; then
    echo "CI builds these standalone but does not test them as opt-in features:" >&2
    sed 's/^/  /' "$tmp/ci_only_standalone" >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    count=$(wc -l < "$tmp/ci_features" | tr -d ' ')
    echo "local gate lists agree with CI ($count plugins)"
fi

exit "$status"
