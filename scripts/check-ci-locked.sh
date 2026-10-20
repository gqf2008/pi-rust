#!/bin/sh
# Assert every dependency-resolving cargo command in CI pins the lockfile.
#
# Why this is a guard and not a comment: `windows-check` ran
# `cargo check --workspace --all-targets` for its whole life while the other three
# jobs passed `--locked`. That asymmetry is invisible in review -- both forms look
# like "check the workspace" -- but it changes what a red X means. Unpinned, the
# resolver may select a dependency version that no other job uses (and that is not
# in Cargo.lock), so a Windows failure could be an upstream release rather than
# this commit. Pinned, the failing commit is the only variable.
#
# `cargo fmt` resolves nothing, so it is exempt. `cargo install` is not used here;
# release/build scripts live outside ci.yml and are covered by their own tests.
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# Scan every workflow, not just ci.yml: a new release/build workflow is exactly the
# place where someone writes `cargo build --release` and forgets the flag.
WF_DIR="$ROOT/.github/workflows"

[ -d "$WF_DIR" ] || { echo "FAIL: $WF_DIR not found" >&2; exit 1; }

workflows=$(ls "$WF_DIR"/*.yml "$WF_DIR"/*.yaml 2>/dev/null)
[ -n "$workflows" ] || { echo "FAIL: no workflow files in $WF_DIR" >&2; exit 1; }

# Join backslash-continued lines so `cargo build --locked \` + `-p foo` is one command.
# Strip full-line YAML comments first: they discuss past failures ("`cargo build --features
# webp` failed") and would otherwise be reported as commands.
joined=""
for wf in $workflows; do
    joined="$joined
$(grep -vE '^[[:space:]]*#' "$wf" \
    | awk '{ if (sub(/\\[[:space:]]*$/, "")) printf "%s ", $0; else print $0 }')"
done

# A silently-empty match would make this check vacuous.
if ! printf '%s\n' "$joined" | grep -qE 'cargo (check|test|build|run|clippy)'; then
    echo "FAIL: no cargo build/check/test commands found in $CI" >&2
    echo "      (the parser probably broke, not CI)" >&2
    exit 1
fi

status=0
found=0

while IFS= read -r clean; do
    [ -n "$clean" ] || continue
    found=$((found + 1))
    case "$clean" in
        *--locked*) echo "  locked: $clean" ;;
        *)
            echo "FAIL: CI runs a dependency-resolving cargo command without --locked:" >&2
            echo "      $clean" >&2
            echo "      Every such command must pin Cargo.lock so a failure names the commit." >&2
            status=1
            ;;
    esac
done <<EOF
$(printf '%s\n' "$joined" | grep -oE 'cargo (check|test|build|run|clippy)[^&|;#]*' | sed 's/[[:space:]]*$//')
EOF

if [ "$found" -eq 0 ]; then
    echo "FAIL: matched zero cargo commands; the parser is broken" >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    echo "ci: every cargo build/check/test command pins the lockfile ($found commands)"
fi
exit "$status"
