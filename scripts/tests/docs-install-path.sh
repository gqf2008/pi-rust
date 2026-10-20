#!/bin/sh
# Keep the README's install instructions consistent with what the crates allow.
#
# Every crate sets `publish = false`, and README.md says so in its own header ("the
# crates are **not** published to crates.io"). Despite that the document used to:
#
#   * carry crates.io / downloads / docs.rs badges for `kiqi-cli` (all render as
#     "not found"),
#   * tell users to run `cargo install kiqi-cli` (both as the quick-start command and
#     in a "From crates.io" section),
#   * link every crate name to `https://crates.io/crates/kiqi-*`,
#   * close the crate table with "Every crate has its own README and docs.rs page".
#
# A user following any of those gets an error or a missing page, and the contradiction
# is inside one file — so it is checkable without running anything.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
status=0

# The manifests are the authority: if they ever become publishable, these checks
# should be revisited rather than deleted.
unpublished=0
published=0
for manifest in "$ROOT"/crates/*/Cargo.toml; do
    if grep -q '^publish = false' "$manifest"; then
        unpublished=$((unpublished + 1))
    else
        published=$((published + 1))
    fi
done

if [ "$unpublished" -gt 0 ] && [ "$published" -gt 0 ]; then
    echo "note: some crates are published and some are not ($published/$unpublished);"
    echo "      the README check below assumes none are"
fi

for doc in "$ROOT/README.md" "$ROOT/docs/kiqi-tool/README.md"; do
    [ -f "$doc" ] || continue
    rel=${doc#"$ROOT"/}

    # No crates.io badges for a crate that is not on crates.io.
    grep -q 'img.shields.io/crates/v/kiqi-cli' "$doc" && {
        echo "FAIL: $rel has a crates.io version badge for an unpublished crate" >&2
        status=1
    }
    # No link to a crates.io crate page (the doc's own prose explains there is none).
    grep -q 'crates.io/crates/kiqi-' "$doc" && {
        echo "FAIL: $rel links to crates.io crate pages; nothing is published there" >&2
        status=1
    }

    if [ "$published" -eq 0 ]; then
        # A bare `cargo install <crate>` cannot work with publish = false. Mentioning it
        # in prose ("does not work") is fine; presenting it as an instruction is not.
        if grep -n 'cargo install kiqi-cli' "$doc" | grep -v 'does not work' >/dev/null; then
            echo "FAIL: $rel instructs 'cargo install kiqi-cli', which cannot succeed" >&2
            status=1
        fi
        grep -q 'Every crate has its own README and docs.rs page' "$doc" && {
            echo "FAIL: $rel claims a docs.rs page for every crate; none exists" >&2
            status=1
        }
        # The supported source build must be present instead.
        grep -q 'cargo install --path crates/kiqi-cli' "$doc" || {
            echo "FAIL: $rel does not show the working source-install command" >&2
            status=1
        }
    fi
done

# --- User-facing docs must describe THIS repository --------------------------
# The other repo is not an older copy of this one: measured, it has 10 releases at
# v0.6.1 with `rpi-v*` assets, while this one has 2 at v0.4.1 with `ki-v*`. Its
# `scripts/install.sh` sets REPO="bigfish1913/pi-rust", and its copy of
# docs/extensions/authoring.md is an `rpi` document (569 lines vs our 399). Every such
# URL answers 200, so nothing errors -- the reader just gets a different tool.
#
# Two kinds of exemption, both deliberate:
#   1. files whose mentions are historical record (rewriting falsifies provenance);
#   2. lines that name it *as* the other project (the header note, star-history chart).
is_exempt_file() {
    case "$1" in
        CHANGELOG.md) return 0 ;;
        docs/promotion/*) return 0 ;;
        docs/design/*) return 0 ;;
        crates/kiqi-cli/embedded-docs/changelog.md) return 0 ;;
        AGENTS.md) return 0 ;;
        plugins/*/DESIGN.md) return 0 ;;
    esac
    return 1
}

# Install scripts count as user-facing docs: `install.sh --help` prints its own header
# verbatim, so a stale URL there is a copy-pasteable instruction even though the file
# is not Markdown. Same for the issue templates (they send users to a repository).
cd "$ROOT" || exit 1
for rel in $(git ls-files '*.md' 'scripts/install.sh' 'scripts/install.ps1' \
    'scripts/tests/install.Tests.ps1' \
    '.github/ISSUE_TEMPLATE/*.yml' 'website/*.json' 2>/dev/null); do
    is_exempt_file "$rel" && continue
    [ -f "$rel" ] || continue
    grep -n 'bigfish1913/pi-rust' "$rel" \
        | grep -vE 'star-history|Upstream|cherry-pick' >/dev/null && {
        echo "FAIL: $rel links to bigfish1913/pi-rust," >&2
        echo "      which serves a different project (rpi v0.6.1, rpi-v* assets)" >&2
        status=1
    }
done

if [ "$status" -eq 0 ]; then
    echo "docs: install instructions match the manifests ($unpublished crates, publish = false)"
fi
exit "$status"
