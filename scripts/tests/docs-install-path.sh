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

# The user guides and the crate README are read at exactly the moment the
# instruction matters (the first command a new user copies), so they are held
# to the same rule as the top-level README. The crate README used to ship
# `cargo install kiqi-cli` plus crates.io/docs.rs badges for a crate that is
# not published; the guides' first install snippet was the same dead command.
for doc in \
    "$ROOT/README.md" \
    "$ROOT/docs/kiqi-tool/README.md" \
    "$ROOT/docs/kiqi-tool/user-guide.md" \
    "$ROOT/docs/guides/user-guide.md" \
    "$ROOT/crates/kiqi-cli/README.md"
do
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
        if grep -n 'cargo install kiqi-cli' "$doc" \
            | grep -vE 'does not work|装不到' >/dev/null; then
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

    # `ki install <name>` resolves the crate through crates.io (install.rs's header
    # describes the temporary workspace it builds), so without `--path` it can only
    # work for a published crate. A first-party extension name there is doubly wrong:
    # those crates were never published and were deleted with the compile-time
    # channel on 2026-10-21. The wasm guest is the supported spelling now.
    if grep -n 'ki install kiqi-' "$doc" | grep -v -- '--path' >/dev/null; then
        echo "FAIL: $rel tells users to 'ki install kiqi-<name>' from crates.io," >&2
        echo "      which resolves nothing; first-party extensions are wasm (ki insmod --build)" >&2
        grep -n 'ki install kiqi-' "$doc" | grep -v -- '--path' | head -5 | sed 's/^/      /' >&2
        status=1
    fi
done

# --- The extension channel must be described as it exists -----------------------
# First-party extensions are wasm components (extensions/wasm/kiqi-*, contract in
# extensions/kiqi-extension.wit); the compile-time `extensions/kiqi-*` crates, their
# `extension-kiqi-*` features and their cdylib build were retired. The README kept the
# old description anyway: its only install example was `ki install kiqi-todo`
# (`build from crates.io`), a crate that was unpublished and then deleted, and the
# word `insmod` did not appear anywhere in the file. Following it gives
# "package not found" and leaves the reader unaware that anything runs at all.
for doc in "$ROOT/README.md" "$ROOT/docs/kiqi-tool/README.md"; do
    [ -f "$doc" ] || continue
    rel=${doc#"$ROOT"/}
    grep -q 'ki insmod --build extensions/wasm/kiqi-' "$doc" || {
        echo "FAIL: $rel does not show the wasm install path ('ki insmod --build ...')" >&2
        status=1
    }
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
#   2. lines that name it explicitly *as* the other project ("… is an unrelated
#      project/fork", the header note, the star-history chart).
is_exempt_file() {
    case "$1" in
        CHANGELOG.md) return 0 ;;
        docs/promotion/*) return 0 ;;
        docs/design/*) return 0 ;;
        crates/kiqi-cli/embedded-docs/changelog.md) return 0 ;;
        AGENTS.md) return 0 ;;
        extensions/*/DESIGN.md) return 0 ;;
    esac
    return 1
}

# Install scripts count as user-facing docs: `install.sh --help` prints its own header
# verbatim, so a stale URL there is a copy-pasteable instruction even though the file
# is not Markdown. Same for the issue templates (they send users to a repository).
cd "$ROOT" || exit 1
for rel in $(git ls-files '*.md' 'scripts/install.sh' 'scripts/install.ps1' \
    'scripts/tests/install.Tests.ps1' \
    '.github/ISSUE_TEMPLATE/*.yml' 2>/dev/null); do
    is_exempt_file "$rel" && continue
    [ -f "$rel" ] || continue
    grep -n 'bigfish1913/pi-rust' "$rel" \
        | grep -vE 'star-history|Upstream|cherry-pick|unrelated (fork|project)' >/dev/null && {
        echo "FAIL: $rel links to bigfish1913/pi-rust," >&2
        echo "      which serves a different project (rpi v0.6.1, rpi-v* assets)" >&2
        status=1
    }
done

# --- Only name packages that exist --------------------------------------------
# `ki dev --package <name>` / `-P <name>` resolves the name through Cargo, so a
# name that is not a workspace member fails with "package not found". The docs and
# the website used `ki dev --package kiqi-todo`, `ki dev -P kiqi-webfetch` and
# `ki dev --package kiqi-todo -- --model gateway/model` in eight files - all names
# of compile-time extension crates deleted with the retired channel (d7297b0), none of
# them in `Cargo.toml`'s member list. The examples now use `my-ext-a`/`my-ext-b`
# and say they are placeholders.
#
# The rule is blunt on purpose: no workspace member is a `kiqi-*` cdylib any more,
# so any `ki dev --package kiqi-...` is stale by construction.
cd "$ROOT" || exit 1
for rel in $(git ls-files 'docs/*.md' \
    'docs/*/*.md' 'crates/*/src/*.rs' 'crates/*/*.md' 2>/dev/null); do
    is_exempt_file "$rel" && continue
    [ -f "$rel" ] || continue
    dead_dev='ki dev \(--package\|-P\) kiqi-'
    grep -n "$dead_dev" "$rel" >/dev/null && {
        echo "FAIL: $rel shows 'ki dev --package kiqi-<name>'; no such package exists" >&2
        grep -n "$dead_dev" "$rel" | head -3 | sed 's/^/      /' >&2
        status=1
    }
done

# Source comments citing a deleted extension file send the next reader to a path that
# cannot be opened. `crates/kiqi-cli/src/extensions.rs` cited
# `extensions/kiqi-goal/src/lib.rs:134-138` for the goal's `current_dir()` write.
# `docs/design/*` keeps such paths as historical record (each file carries the
# 2026-10-21 retirement note) and is exempt above.
for rel in $(git ls-files 'crates/*/src/*.rs' 'crates/*/src/*/*.rs' 2>/dev/null); do
    [ -f "$rel" ] || continue
    grep -n 'extensions/kiqi-[a-z-]*/src' "$rel" >/dev/null && {
        echo "FAIL: $rel cites a deleted extension source path (extensions/kiqi-*/src)" >&2
        grep -n 'extensions/kiqi-[a-z-]*/src' "$rel" | head -3 | sed 's/^/      /' >&2
        status=1
    }
done

if [ "$status" -eq 0 ]; then
    echo "docs: install instructions match the manifests ($unpublished crates, publish = false)"
fi
exit "$status"
