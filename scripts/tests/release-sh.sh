#!/bin/sh
# Keep `release.sh` consistent with the workflow it orchestrates.
#
# Two classes of drift, both silent and both found in this script:
#
#   1. **Push target.** `phase_push` ran `git push origin HEAD`. At the time,
#      `origin` pointed at the unrelated `bigfish1913/pi-rust`, which AGENTS.md
#      forbids pushing to. A release would have published to the wrong repo.
#   2. **Asset names.** `phase_wait` polled for `kiqi-v<ver>-<target>`, but
#      `release-binaries.yml` uploads `ki-v<ver>-<target>` (`name="ki-v${version}-
#      ${target}"`). The poll would never see its files and would die after
#      120 × 15 s — 30 minutes of waiting for something that cannot appear.
#
# Both are checked statically: the workflow is the source of truth for the asset
# name, and AGENTS.md for the remote. Nothing here runs a release.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
RELEASE="$ROOT/scripts/release.sh"
WORKFLOW="$ROOT/.github/workflows/release-binaries.yml"
AGENTS="$ROOT/AGENTS.md"

for f in "$RELEASE" "$WORKFLOW" "$AGENTS"; do
    [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
done

status=0

# --- 1. The workflow's asset name is `ki-v<version>-<target>` ----------------
# Assert the workflow still says so, then that release.sh expects the same prefix.
grep -q 'name="ki-v\${version}-\${target}"' "$WORKFLOW" || {
    echo "FAIL: release-binaries.yml no longer names assets 'ki-v<version>-<target>';" >&2
    echo "      update this test and scripts/release.sh together" >&2
    status=1
}
grep -q 'needed+=("ki-v\$release_version-' "$RELEASE" || {
    echo "FAIL: scripts/release.sh does not poll for 'ki-v<version>-…' assets" >&2
    status=1
}
grep -q 'needed+=("kiqi-' "$RELEASE" && {
    echo "FAIL: scripts/release.sh still polls the retired 'kiqi-v…' asset names" >&2
    status=1
}

# --- 1b. refresh-channels.sh builds the same asset names --------------------
# Same drift class as release.sh: it constructs the names itself, so a rename has to
# reach this file too.
REFRESH="$ROOT/scripts/refresh-channels.sh"
if [ -f "$REFRESH" ]; then
    # Match code only: the explanatory comment above those lines quotes the old
    # names, and a whole-file grep would be satisfied by that prose — the same
    # "checker that cannot fail" trap this repo has hit before.
    grep -v '^[[:space:]]*#' "$REFRESH" | grep -q 'kiqi-\$TAG-' && {
        echo "FAIL: refresh-channels.sh still builds 'kiqi-\$TAG-…' asset names" >&2
        echo "      the workflow uploads 'ki-v<version>-<target>'" >&2
        status=1
    }
    grep -q 'ki-v\$VERSION-' "$REFRESH" || {
        echo "FAIL: refresh-channels.sh does not build 'ki-v\$VERSION-…' asset names" >&2
        status=1
    }
fi

# --- 1c. Channel manifests must not embed the retired prefix ----------------
# The manifests carry full URLs, so the rename has to reach them too. Their *repo*
# and *version* are a separate question (see the `release-target-split` card); this
# checks only the asset-name prefix owned by release-binaries.yml.
# `scripts/install.{sh,ps1}` belong here too: they build the same asset name, and
# `install.ps1` carried `kiqi-v$Version-$Target.zip` until 2026-10 — a name that
# 404s on both repositories, making the Windows install path impossible while the
# PowerShell twin test passed (it only drove `-DryRun` shapes).
for manifest in \
    "$ROOT/packaging/homebrew/ki.rb" \
    "$ROOT/packaging/scoop/ki.json" \
    "$ROOT/packaging/winget/gqf2008.ki.installer.yaml" \
    "$ROOT/scripts/install.sh" \
    "$ROOT/scripts/install.ps1" \
    "$ROOT/scripts/tests/install.Tests.ps1"
do
    [ -f "$manifest" ] || continue
    rel=${manifest#"$ROOT"/}
    # Comment lines first: the v0.3.2 digests table quotes the old names on purpose.
    grep -v '^[[:space:]]*#' "$manifest" | grep -q 'kiqi-v' && {
        echo "FAIL: $rel still embeds 'kiqi-v…' (the workflow uploads 'ki-v…')" >&2
        status=1
    }
done

# --- 2. Pushing goes to `origin` (gqf2008/pi-rust, the GitHub mirror) ------
# 2026-10-27: the old `fork` remote was renamed to `origin` and now points at
# the project's own GitHub repo (the unrelated bigfish1913/pi-rust fork has
# been removed from the config). `origin` is correct here.
grep -q 'PUSH_REMOTE=.*origin' "$RELEASE" || {
    echo "note: release.sh no longer defaults to origin; review this test" >&2
}
grep -q 'git push "\$PUSH_REMOTE" HEAD' "$RELEASE" || {
    echo "FAIL: scripts/release.sh does not push the branch to \$PUSH_REMOTE" >&2
    status=1
}
grep -q 'PUSH_REMOTE="\${KI_RELEASE_REMOTE:-origin}"' "$RELEASE" || {
    echo "FAIL: \$PUSH_REMOTE must default to 'origin'" >&2
    status=1
}

# --- 3. The release target split is resolved: gqf2008 everywhere ----------
# `release-target-split` found the pipeline publishing to an unrelated fork while
# every runtime consumer read gqf2008. Keep the two sides from drifting apart again.
for f in scripts/release.sh scripts/refresh-channels.sh; do
    grep -q 'REPO="gqf2008/pi-rust"' "$ROOT/$f" || {
        echo "FAIL: $f does not target gqf2008/pi-rust" >&2
        status=1
    }
done
if grep -v '^[[:space:]]*#' "$ROOT/scripts/push-taps.sh" | grep -q 'bigfish1913'; then
    echo "FAIL: scripts/push-taps.sh still names bigfish1913 (unrelated fork)" >&2
    status=1
fi
for manifest in     "$ROOT/packaging/homebrew/ki.rb"     "$ROOT/packaging/scoop/ki.json"     "$ROOT/packaging/winget/gqf2008.ki.yaml"     "$ROOT/packaging/winget/gqf2008.ki.installer.yaml"     "$ROOT/packaging/winget/gqf2008.ki.locale.en-US.yaml"
do
    [ -f "$manifest" ] || { echo "FAIL: missing $manifest" >&2; status=1; continue; }
    grep -v '^[[:space:]]*#' "$manifest" | grep -q 'bigfish1913' && {
        echo "FAIL: ${manifest#"$ROOT"/} still points at bigfish1913" >&2
        status=1
    }
done
grep -q 'PackageIdentifier: gqf2008.ki' "$ROOT/packaging/winget/gqf2008.ki.yaml" || {
    echo "FAIL: the winget identifier is not gqf2008.ki" >&2
    status=1
}

# --- 4. Untracked files cannot ride a release, and the refresh is pushed -----
# Preflight used to ignore untracked files (`--untracked-files=no`) while
# `phase_commit` staged with `-A`: a stray `.env` in the checkout passed
# preflight and was published with the tag. And the channel-refresh commit is
# created *after* `phase_push`, so without a push of its own a pipeline-only
# release left the refreshed manifests local while origin/main kept the old
# packaging.
phase_body() {
    sed -n "/^phase_$1() {/,/^}/p" "$RELEASE"
}
if grep -v '^[[:space:]]*#' "$RELEASE" | grep -q -- '--untracked-files=no'; then
    echo "FAIL: preflight still hides untracked files while phase_commit stages them" >&2
    status=1
fi
phase_body commit | grep -q 'git add -u' || {
    echo "FAIL: phase_commit does not stage tracked modifications with 'git add -u'" >&2
    status=1
}
if phase_body commit | grep -q 'git add -A'; then
    echo "FAIL: phase_commit still stages everything with 'git add -A'" >&2
    status=1
fi
phase_body commit_channels | grep -q 'git add packaging/' || {
    echo "FAIL: phase_commit_channels does not stage only packaging/" >&2
    status=1
}
phase_body commit_channels | grep -q 'git push "\$PUSH_REMOTE" HEAD' || {
    echo "FAIL: phase_commit_channels never pushes the refresh commit" >&2
    status=1
}
# The consistency gate now also checks the packaging manifests, which the
# channels phase refreshes *after* validate; the validate step must declare that
# they are still pending, or every release cut fails on its own tag.
phase_body validate | grep -q 'KI_RELEASE_PENDING_CHANNELS=1' || {
    echo "FAIL: phase_validate does not mark the channels refresh as pending" >&2
    status=1
}

if [ "$status" -eq 0 ]; then
    echo "release.sh: asset names match the workflow, push target is 'origin', channels target gqf2008"
fi
exit "$status"
