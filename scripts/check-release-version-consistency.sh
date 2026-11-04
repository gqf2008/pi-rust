#!/bin/sh
# Assert every "current release" claim names the workspace version.
#
# Why: the version is written down by hand in three places -- the README status
# table, the docs-tool README's copy of that table, and ROADMAP.md's opening
# line. `release.sh` bumps Cargo.toml but nothing reads these rows, so they
# drift silently: through 0.5.0-0.5.3 all three still said 0.4.7, and the
# embedded copy shipped that in `ki docs overview` and the release archives.
#
# Scope is deliberately narrow: only rows that make a claim about the *current*
# release. Historical mentions ("Shipped in 0.4.7"), install examples
# (`-Version 0.4.7`) and changelog headings are not release state and must not
# be forced to move.
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
VERSION=$(awk '
    /^\[workspace\.package\]/ { in_section = 1; next }
    /^\[/ { in_section = 0 }
    in_section && /^version[[:space:]]*=/ {
        sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
    }
' "$ROOT/Cargo.toml")

case "$VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "FAIL: could not read the workspace version from Cargo.toml ('$VERSION')" >&2; exit 1 ;;
esac

status=0

# The two READMEs share one row shape:
#   | Current release | **[0.5.3](https://github.com/gqf2008/pi-rust/releases/tag/v0.5.3)** ...
# Both the displayed number and the tag link are current-release claims, so a
# bump that updates one but not the other is still a drift.
check_table_row() {
    file=$1
    row=$(grep -E '^\|[[:space:]]*Current release[[:space:]]*\|' "$ROOT/$file" | head -n 1)
    if [ -z "$row" ]; then
        echo "FAIL: $file has no '| Current release |' row" >&2
        status=1
        return
    fi
    parsed=$(printf '%s\n' "$row" \
        | sed -E 's/.*\*\*\[([0-9]+\.[0-9]+\.[0-9]+)\]\(([^)]*)\)\*\*.*/\1 \2/')
    case "$parsed" in
        *" "*) ;;
        *) echo "FAIL: $file's current-release row is not parseable: $row" >&2; status=1; return ;;
    esac
    shown=${parsed%% *}
    link=${parsed#* }
    if [ "$shown" != "$VERSION" ]; then
        echo "FAIL: $file says the current release is $shown; the workspace version is $VERSION" >&2
        status=1
    fi
    case "$link" in
        */releases/tag/v"$VERSION") ;;
        *) echo "FAIL: $file's current-release row links $link; expected a v$VERSION tag" >&2; status=1 ;;
    esac
}

check_table_row "README.md"
check_table_row "docs/kiqi-tool/README.md"

# ROADMAP.md states it as prose: `Current release: **0.5.3**.`
roadmap=$(grep -E '^Current release:' "$ROOT/ROADMAP.md" | head -n 1 \
    | sed -E 's/^Current release:[[:space:]]*\*\*([0-9]+\.[0-9]+\.[0-9]+)\*\*.*/\1/')
case "$roadmap" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "FAIL: ROADMAP.md has no parseable 'Current release: **X.Y.Z**' line" >&2; status=1; roadmap="" ;;
esac
if [ -n "$roadmap" ] && [ "$roadmap" != "$VERSION" ]; then
    echo "FAIL: ROADMAP.md says the current release is $roadmap; the workspace version is $VERSION" >&2
    status=1
fi

# Package-manager prose must not hard-code a version.
#
# The README used to say the taps "carry ki 0.5.3" for three releases after that
# was false, because nothing scanned the sentence: the row checks above only
# look at `| Current release |`. A version belongs in that row; the tap sentence
# says "the current release".
for file in README.md docs/kiqi-tool/README.md; do
    hits=$(grep -nE 'tap|brew|scoop' "$ROOT/$file" \
        | grep -E '[0-9]+\.[0-9]+\.[0-9]+' \
        | grep -v 'Current release' || true)
    if [ -n "$hits" ]; then
        echo "FAIL: $file hard-codes a version in a package-manager sentence (say 'the current release'):" >&2
        printf '%s\n' "$hits" | sed 's/^/      /' >&2
        status=1
    fi
done

# The channel docs must describe the release that actually exists.
#
# `release.sh` refreshes the channels *after* `phase_validate`, so during a cut
# the packaging tree still names the previous release. The pipeline sets
# KI_RELEASE_PENDING_CHANNELS=1 for that one invocation; CI and every other
# caller get the full check.
if [ "${KI_RELEASE_PENDING_CHANNELS:-0}" = "1" ]; then
    echo "note: channels refresh still pending (KI_RELEASE_PENDING_CHANNELS=1); skipping packaging checks"
else
#   * the digest table's own label: `sha256 (v0.5.6)`
#   * status prose: `published (0.5.6)` / `currently v0.5.6`
label=$(grep -m1 -oE 'sha256 \(v[0-9.]+\)' "$ROOT/packaging/README.md" | sed -E 's/.*\(v//; s/\)$//')
if [ -n "$label" ] && [ "$label" != "$VERSION" ]; then
    echo "FAIL: packaging/README.md's digest table is labelled v$label; the workspace version is $VERSION" >&2
    status=1
fi
claims=$(grep -nE 'published \(v?[0-9]+\.[0-9]+\.[0-9]+\)|currently v[0-9]+\.[0-9]+\.[0-9]+' \
    "$ROOT/packaging/README.md" || true)
if [ -n "$claims" ]; then
    while IFS= read -r line; do
        case "$line" in
            *"$VERSION"*) ;;
            *) echo "FAIL: packaging/README.md names a stale release: $line" >&2; status=1 ;;
        esac
    done <<EOF
$claims
EOF
fi

fi

if [ "$status" -eq 0 ]; then
    echo "current-release claims agree with the workspace version ($VERSION): README.md, docs/kiqi-tool/README.md, ROADMAP.md, packaging/README.md"
fi
exit "$status"
