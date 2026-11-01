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
#
# A target-gated step may still need to run the binary for a target-specific reason --
# the macOS step must exec the *signed* copy, because a bad signature/entitlement pair
# only fails at exec time, and that is how the missing JIT entitlement shipped. Such a
# step opts out explicitly with a `# release-targets: allow-target-gated` comment.
#
# Two things keep that opt-out from swallowing the invariant, both learned by breaking it:
#   1. an exempt block is not counted as coverage, so deleting the ungated smoke step
#      while leaving the exemption behind still fails ("never executes one");
#   2. an exemption on a step that is *not* target-gated is itself a failure -- that is
#      the shape you get by pasting the marker into the smoke step to silence it.
# Indentation is not assumed: a step block is anything starting with `<indent>- name:`,
# and a gate is an `if:` indented *deeper* than its block (a job-level `if:` sits at a
# shallower indent and therefore does not count). Hard-coding 6/8 columns made a
# consistent re-indent of the steps list yield zero parsed blocks -- and zero
# violations is exactly what a broken parser reports.
awk '
    function indent_of(str,    copy) {
        copy = str
        sub(/[^ ].*$/, "", copy)
        return length(copy)
    }
    function flush() {
        if (!nruns) { return }
        if (nexempt) {
            if (ngate) { exempts++ } else { print "EXEMPT-NOT-GATED|" nname }
        } else {
            found = 1
            if (ngate) { print nname "|" gate }
        }
    }
    /^[[:space:]]*-[[:space:]]+name:/ {
        flush()
        nname=$0; nruns=0; ngate=0; gate=""; nexempt=0
        nindent = indent_of($0); nblocks++
        next
    }
    /^[[:space:]]*#[[:space:]]*release-targets:[[:space:]]*allow-target-gated/ { nexempt=1 }
    /^[[:space:]]*"\$\{bin\}"/ { nruns=1 }
    /^[[:space:]]*if:/ { if (indent_of($0) > nindent) { ngate=1; gate=$0 } }
    END { flush(); if (!found) print "NONE|"; print "EXEMPTED|" exempts + 0; print "BLOCKS|" nblocks + 0 }
' "$WF" > "$tmp/gated"

# A parser that silently sees nothing is worse than no check: compare the number of
# blocks it walked against the file itself.
blocks_expected=$(grep -cE '^[[:space:]]*-[[:space:]]+name:' "$WF")
blocks_parsed=$(sed -n 's/^BLOCKS|//p' "$tmp/gated")
if [ "$blocks_parsed" != "$blocks_expected" ]; then
    echo "FAIL: parsed $blocks_parsed step blocks out of $blocks_expected in $WF" >&2
    echo "      the smoke-test check would silently pass on nothing" >&2
    status=1
fi

if grep -q '^NONE|' "$tmp/gated"; then
    echo "FAIL: $WF builds release binaries but never executes one" >&2
    echo "      a binary that cannot start would be uploaded untested" >&2
    status=1
fi

if grep -q '^EXEMPT-NOT-GATED|' "$tmp/gated"; then
    echo "FAIL: 'release-targets: allow-target-gated' is on a step that is not target-gated:" >&2
    grep '^EXEMPT-NOT-GATED|' "$tmp/gated" | sed 's/^EXEMPT-NOT-GATED|/      /' >&2
    echo "      the opt-out exists for target-specific work (signing the macOS binary);" >&2
    echo "      on a step that runs for every target it only hides the smoke test" >&2
    status=1
fi

if grep -vE '^(NONE|EXEMPTED|BLOCKS|EXEMPT-NOT-GATED)\|' "$tmp/gated" | grep -q .; then
    echo "FAIL: the step that executes the release binary is gated on a single target:" >&2
    grep -vE '^(NONE|EXEMPTED|BLOCKS|EXEMPT-NOT-GATED)\|' "$tmp/gated" | sed 's/|/\n      /; s/^/      /' >&2
    echo "      every target is built on a runner that can execute it (Rosetta, when the\n      target differs from the runner architecture)" >&2
    status=1
fi

if [ "$status" -eq 0 ]; then
    exempted=$(sed -n 's/^EXEMPTED|//p' "$tmp/gated")
    echo "release targets agree across matrix, updates.rs, install.sh and install.ps1 ($(wc -l < "$tmp/producer" | tr -d ' ') triples); all are smoke-tested"\
         "${exempted:+($exempted target-gated exception(s))}"
fi
exit "$status"
