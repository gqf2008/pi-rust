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
