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

# --- Install commands must fetch THIS repository -----------------------------
# The other repo's `scripts/install.sh` has `REPO="bigfish1913/pi-rust"` and installs
# an unrelated build (`rpi`, a different version line). Following a README that points
# there gets you somebody else's tool.
for doc in "$ROOT/README.md" "$ROOT/docs/kiqi-tool/README.md"; do
    [ -f "$doc" ] || continue
    rel=${doc#"$ROOT"/}
    grep -n 'bigfish1913/pi-rust' "$doc" \
        | grep -E 'raw\.githubusercontent\.com|releases/download|actions/workflows' \
        | grep -v '^[0-9]*:>' >/dev/null && {
        echo "FAIL: $rel points an install/badge URL at bigfish1913/pi-rust," >&2
        echo "      which serves a different project (rpi, v0.6.1)" >&2
        status=1
    }
done

if [ "$status" -eq 0 ]; then
    echo "docs: install instructions match the manifests ($unpublished crates, publish = false)"
fi
exit "$status"
