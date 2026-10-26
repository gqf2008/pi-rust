#!/bin/sh
# Offline regression test for scripts/check-doc-paths.sh. The checker derives
# its root from its own location, so a temporary fixture can exercise both the
# pass and fail paths without modifying the real documentation.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fixture="$tmp/repo"
mkdir -p "$fixture/scripts" "$fixture/docs/nested" "$fixture/docs/extensions" \
    "$fixture/crates/kiqi-host/src" "$fixture/extensions/wasm/kiqi-todo"
cp "$ROOT/check-doc-paths.sh" "$fixture/scripts/check-doc-paths.sh"

cat > "$fixture/README.md" <<'EOF'
`crates/kiqi-host` and `extensions/wasm/kiqi-todo` exist.
Examples: `crates/CRATE`, `extensions/<id>`, `extensions/wasm/kiqi-*`.
Generated: `extensions/wasm/target/release/example.wasm`.
External paths and URLs are not repository claims: `abb/crates/other`,
`.ki/extensions/<id>`, https://img.shields.io/crates/l/ki.svg.
EOF
cat > "$fixture/docs/nested/current.md" <<'EOF'
[host](../../crates/kiqi-host) is a document-relative link.
EOF
cat > "$fixture/docs/README.md" <<'EOF'
[authoring](extensions/authoring.md) is relative to this file.
EOF
: > "$fixture/docs/extensions/authoring.md"
cat > "$fixture/docs/history.md" <<'EOF'
> 历史存档：旧实施坐标，不对应现在的目录。
`crates/retired` and `extensions/retired` are historical records.
EOF
cat > "$fixture/CHANGELOG.md" <<'EOF'
`crates/retired` was removed.
EOF

if ! sh "$fixture/scripts/check-doc-paths.sh" >/dev/null; then
    echo "FAIL: valid fixture did not pass" >&2
    exit 1
fi
echo "ok: concrete, relative, example, generated, and historical paths"

printf '\nMissing `crates/no-such-crate`.\n' >> "$fixture/README.md"
line=$(wc -l < "$fixture/README.md" | tr -d ' ')
if out=$(sh "$fixture/scripts/check-doc-paths.sh" 2>&1); then
    echo "FAIL: a missing root-relative path passed" >&2
    exit 1
fi
printf '%s\n' "$out" | grep -qF "README.md:$line: missing repository path: crates/no-such-crate" || {
    echo "FAIL: missing-path diagnostic lacks its file and line: $out" >&2
    exit 1
}
echo "ok: missing root-relative path fails with file:line"

# Restore the valid fixture, then break a link relative to a nested document.
sed -i.bak '$d' "$fixture/README.md"
printf '[gone](../../crates/no-such-link)\n' >> "$fixture/docs/nested/current.md"
line=$(wc -l < "$fixture/docs/nested/current.md" | tr -d ' ')
if out=$(sh "$fixture/scripts/check-doc-paths.sh" 2>&1); then
    echo "FAIL: a missing document-relative link passed" >&2
    exit 1
fi
printf '%s\n' "$out" | grep -qF "docs/nested/current.md:$line: missing repository path: ../../crates/no-such-link" || {
    echo "FAIL: relative-link diagnostic is wrong: $out" >&2
    exit 1
}
echo "ok: missing document-relative link fails with file:line"

# Mentioning an archived file is not a self-description and must not exempt a
# current document from checking.
mv "$fixture/docs/nested/current.md" "$fixture/docs/nested/valid.md"
cat > "$fixture/docs/nested/current.md" <<'EOF'
See [the old design](../history.md) (历史存档).
The current pointer is `crates/no-such-current-path`.
EOF
if out=$(sh "$fixture/scripts/check-doc-paths.sh" 2>&1); then
    echo "FAIL: citing a historical document exempted the citing file" >&2
    exit 1
fi
printf '%s\n' "$out" | grep -qF 'docs/nested/current.md:2: missing repository path: crates/no-such-current-path' || {
    echo "FAIL: citing-file diagnostic is wrong: $out" >&2
    exit 1
}
echo "ok: merely citing an archive does not skip current documentation"

# Leave a clean fixture before the next cases.
rm "$fixture/docs/nested/current.md" "$fixture/docs/nested/valid.md"

# A retirement note that scopes only named paths must exempt those globs but
# keep checking the rest of the document.
cat > "$fixture/docs/nested/scoped.md" <<'EOF'
> 注：文中 `extensions/kiqi-*` 路径是历史记录，现行实现在 `extensions/wasm/kiqi-*`。

Current: `crates/kiqi-host`; retired: `extensions/kiqi-retired/src/lib.rs`.
EOF
if ! sh "$fixture/scripts/check-doc-paths.sh" >/dev/null 2>&1; then
    echo "FAIL: a scoped retirement note exempted a current path" >&2
    exit 1
fi
echo "ok: a scoped retirement note exempts only its named historical globs"

printf 'Missing `crates/no-such-scoped`.\n' >> "$fixture/docs/nested/scoped.md"
line=$(wc -l < "$fixture/docs/nested/scoped.md" | tr -d ' ')
if out=$(sh "$fixture/scripts/check-doc-paths.sh" 2>&1); then
    echo "FAIL: a scoped retirement note exempted the whole document" >&2
    exit 1
fi
printf '%s\n' "$out" | grep -qF "docs/nested/scoped.md:$line: missing repository path: crates/no-such-scoped" || {
    echo "FAIL: scoped-document diagnostic is wrong: $out" >&2
    exit 1
}
echo "ok: paths outside a retirement note's scope still fail"
rm "$fixture/docs/nested/scoped.md"

# The embedded documentation snapshot is scanned, and its repo-root-style
# links resolve from the repository root rather than the copy's directory.
mkdir -p "$fixture/crates/kiqi-cli/embedded-docs"
cat > "$fixture/crates/kiqi-cli/embedded-docs/README.md" <<'EOF'
[host](crates/kiqi-host) and [guest](extensions/wasm/kiqi-todo) are root-relative.
EOF
if ! sh "$fixture/scripts/check-doc-paths.sh" >/dev/null 2>&1; then
    echo "FAIL: embedded-docs root-relative links did not resolve" >&2
    exit 1
fi
echo "ok: embedded-docs copies are scanned with root-relative links"

printf '\n[gone](crates/no-such-embedded)\n' >> "$fixture/crates/kiqi-cli/embedded-docs/README.md"
line=$(wc -l < "$fixture/crates/kiqi-cli/embedded-docs/README.md" | tr -d ' ')
if out=$(sh "$fixture/scripts/check-doc-paths.sh" 2>&1); then
    echo "FAIL: a missing path in the embedded docs copy passed" >&2
    exit 1
fi
printf '%s\n' "$out" | grep -qF "crates/kiqi-cli/embedded-docs/README.md:$line: missing repository path: crates/no-such-embedded" || {
    echo "FAIL: embedded-docs diagnostic is wrong: $out" >&2
    exit 1
}
echo "ok: a missing path in the embedded docs copy fails"
