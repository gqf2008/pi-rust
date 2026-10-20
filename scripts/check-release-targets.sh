#!/bin/sh
# Assert the release matrix and every consumer agree on the target list.
#
# Why: four independent lists name the same five triples --
#
#   producer   .github/workflows/release-binaries.yml   (matrix.target)
#   consumer   crates/kiqi-cli/src/updates.rs           (release_target_triple)
#   consumer   scripts/install.sh                       (accepted-triple case arm)
#   consumer   scripts/install.ps1                      ($Target, Windows only)
#
# Nothing tested their agreement, so adding a target to the matrix without touching the
# consumers produces a release that exists on GitHub and that no install path can ask
# for -- or the reverse: a consumer requests an asset nobody uploads, and the user sees
# "404" for a platform the project claims to support. Both directions are silent.
#
# `install.ps1` is checked as a subset, because it is the Windows-only installer.
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
WF="$ROOT/.github/workflows/release-binaries.yml"
UPD="$ROOT/crates/kiqi-cli/src/updates.rs"
SH="$ROOT/scripts/install.sh"
PS1="$ROOT/scripts/install.ps1"

for f in "$WF" "$UPD" "$SH" "$PS1"; do
    [ -f "$f" ] || { echo "FAIL: $f not found" >&2; exit 1; }
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

# A triple has *three or more* dash-separated parts: `aarch64-apple-darwin` has three,
# `x86_64-unknown-linux-gnu` has four. Pinning the count at three silently kept only the
# Apple entries and made this check compare a 2-line list against a 5-line one.
triple_re='[a-z0-9_]+(-[a-z0-9_]+){2,}'

collect() { sort -u; }

# Producer. Only the `target:` keys inside the matrix `include:` block.
grep -E '^[[:space:]]+target: ' "$WF" | sed -E 's/.*target:[[:space:]]*//' \
    | grep -xE "$triple_re" | collect > "$tmp/producer"
[ -s "$tmp/producer" ] || { echo "FAIL: no targets parsed from $WF" >&2; exit 1; }

# updates.rs: the string literals returned by release_target_triple.
sed -n '/fn release_target_triple/,/^}/p' "$UPD" \
    | grep -oE "\"$triple_re\"" | tr -d '"' | collect > "$tmp/updates"
[ -s "$tmp/updates" ] || { echo "FAIL: no targets parsed from release_target_triple" >&2; exit 1; }

# install.sh: the case arm that rejects unsupported platforms. It is written as one
# indented line of `a|b|c) ;;`, so split on the pipe.
grep -E '^[[:space:]]+.*\)[[:space:]]*;;' "$SH" \
    | grep -F -- '-' \
    | tr '|)' '\n\n' \
    | sed 's/^[[:space:]]*//; s/[[:space:]]*$//' \
    | grep -xE "$triple_re" | collect > "$tmp/shell"
[ -s "$tmp/shell" ] || { echo "FAIL: no targets parsed from $SH" >&2; exit 1; }

# install.ps1: a single $Target assignment.
grep -E "^[[:space:]]*\\\$Target[[:space:]]*=[[:space:]]*'" "$PS1" \
    | sed -E "s/.*'([^']+)'.*/\1/" | collect > "$tmp/ps1"
[ -s "$tmp/ps1" ] || { echo "FAIL: no \$Target parsed from $PS1" >&2; exit 1; }

status=0

compare() {
    name=$1; left=$2; right=$3
    if ! diff -u "$left" "$right" > "$tmp/diff" 2>&1; then
        echo "FAIL: $name disagrees with the release matrix:" >&2
        sed -n '3,$p' "$tmp/diff" | sed 's/^/      /' >&2
        status=1
    fi
}

compare "crates/kiqi-cli/src/updates.rs" "$tmp/producer" "$tmp/updates"
compare "scripts/install.sh" "$tmp/producer" "$tmp/shell"

# Windows-only installer: every triple it names must be one the matrix builds, but it is
# not required to cover Linux/macOS.
while IFS= read -r t; do
    grep -qxF "$t" "$tmp/producer" || {
        echo "FAIL: scripts/install.ps1 requests '$t', which the matrix does not build" >&2
        status=1
    }
done < "$tmp/ps1"

# --- the PowerShell test fixture must name the same asset ---------------------
# `install.Tests.ps1` cannot run here (no `pwsh` on CI's Unix jobs -- it runs only in the
# Windows job), so a rename can pass every local gate and fail there. That happened:
# changing `install.ps1` to `ki-v$Version-$Target.zip` left the fixture's checksum naming
# `kiqi-v0.3.19-...`, which the installer rejects as "Invalid SHA-256 checksum file" -- the
# test would have failed while testing something other than what it claims.
#
# Compare the two templates with variable names normalised away, so both sides must agree
# on the literal prefix and suffix `$Asset` is built from.
ps1_test="$ROOT/scripts/tests/install.Tests.ps1"
if [ -f "$ps1_test" ]; then
    ps1_tmpl() {
        # `$Asset = "ki-v$Version-$Target.zip"` -> `ki-v$X-$X.zip`
        grep -E '^[[:space:]]*\$[A-Za-z_]*[Aa]sset[[:space:]]*=' "$1" 2>/dev/null \
            | head -n 1 \
            | sed -E 's/^[^"'"'"']*["'"'"']//; s/["'"'"'][[:space:]]*$//' \
            | sed -E 's/\$[A-Za-z_][A-Za-z0-9_]*/$X/g'
    }
    tmpl_installer=$(ps1_tmpl "$PS1")
    tmpl_test=$(ps1_tmpl "$ps1_test")
    if [ -z "$tmpl_installer" ] || [ -z "$tmpl_test" ]; then
        echo "FAIL: could not parse the asset template from install.ps1 ('$tmpl_installer')" >&2
        echo "      or from install.Tests.ps1 ('$tmpl_test')" >&2
        status=1
    elif [ "$tmpl_installer" != "$tmpl_test" ]; then
        echo "FAIL: the Windows installer and its test fixture disagree on the asset name:" >&2
        echo "      install.ps1        $tmpl_installer" >&2
        echo "      install.Tests.ps1  $tmpl_test" >&2
        echo "      the fixture's checksum must name the file the installer requests" >&2
        status=1
    fi
fi

# --- every target must be executed before it is uploaded ----------------------
# Each matrix entry builds on a native runner of its own architecture, so an `if:` on the
# step that *runs* the binary would leave the rest shipped-but-never-started. That is
# exactly how `x86_64-apple-darwin` came to be the only smoke-tested binary of five.
#
# Note the scope: steps that are legitimately target-specific (macOS signing/notarizing)
# keep their `if:`, so this looks only at the block that executes `release/ki`.
# Accumulate one step block at a time (blocks start at six-space `- name:`) and judge the
# block as a whole, because `if:` sits above `run:`.
#
# "Executes the binary" means a line that *invokes* it -- `"${bin}" --version` -- not a
# line that merely mentions the path. The signing step also references
# `target/<t>/release/ki` (it signs that file) and is legitimately target-gated, so a
# looser marker flags Apple signing as a violation.
awk '
    function flush() {
        if (nruns) { found = 1; if (ngate) print nname "|" gate }
    }
    /^      - name:/ { flush(); nname=$0; nruns=0; ngate=0; gate=""; next }
    /^[[:space:]]*"\$\{bin\}"/ { nruns=1 }
    /^        if:/ { ngate=1; gate=$0 }
    END { flush(); if (!found) print "NONE|" }
' "$WF" > "$tmp/gated"

if grep -q '^NONE|' "$tmp/gated"; then
    echo "FAIL: $WF builds release binaries but never executes one" >&2
    echo "      a binary that cannot start would be uploaded untested" >&2
    status=1
fi

if grep -v '^NONE|' "$tmp/gated" | grep -q .; then
    echo "FAIL: the step that executes the release binary is gated on a single target:" >&2
    grep -v '^NONE|' "$tmp/gated" | sed 's/|/\n      /; s/^/      /' >&2
    echo "      every target builds natively, so each can be executed" >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    echo "release targets agree across matrix, updates.rs, install.sh and install.ps1 ($(wc -l < "$tmp/producer" | tr -d ' ') triples); all are smoke-tested"
fi
exit "$status"
