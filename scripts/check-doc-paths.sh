#!/bin/sh
# Check concrete crate/extension repository paths cited by current documentation.
#
# Scope: the root README/CLAUDE/AGENTS files and docs/**/*.md. CHANGELOG files
# and documents with an explicit self-description as historical records are
# skipped: "文中…历史记录", "本文…历史存档/历史契约/设计存档", or a
# leading "历史存档:" / "historical archive:" note. Merely citing an archived
# document does not exempt the citing file.
#
# Only paths which must exist are checked:
#   * wildcards (`extensions/wasm/kiqi-*`) and placeholders (`extensions/<id>`,
#     `crates/CRATE`, or the `extensions/wasm/kiqi-` name prefix) are examples;
#   * `crates/kiqi-cli/.ki`, `extensions/conformance/extension.wasm`, and
#     `extensions/wasm/target` are runtime/test/build output, not source files;
#   * paths below another directory (`.ki/extensions/...`, `docs/extensions/...`,
#     `abb/crates/...`) and crates.io/shields URLs are not root-relative claims.
# Markdown link destinations are resolved from the document's directory; paths
# in commands and inline code are resolved from the repository root. examples/
# is intentionally outside this check because its examples need not all exist.
set -eu
# Some forensic notes contain arbitrary bytes; byte-wise matching is sufficient
# for ASCII paths and keeps the checker portable to macOS awk.
LC_ALL=C
export LC_ALL

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

[ -d docs ] || { echo "FAIL: docs directory not found" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
: > "$tmp/docs"
: > "$tmp/refs"
: > "$tmp/missing"

for doc in README.md CLAUDE.md AGENTS.md; do
    [ ! -f "$doc" ] || printf '%s\n' "$doc" >> "$tmp/docs"
done
find docs -type f -name '*.md' -print | sort >> "$tmp/docs"

while IFS= read -r doc; do
    case "$doc" in
        CHANGELOG.md|*/CHANGELOG.md|changelog.md|*/changelog.md) continue ;;
    esac
    if grep -qiE '文中.*历史记录|本文.*(历史存档|历史契约|设计存档)|^>[[:space:]]*(历史存档：|historical archive:)' "$doc"; then
        continue
    fi

    # Find every candidate, including several on one line. AWK lets us reject
    # candidates embedded in a longer path without requiring non-POSIX grep.
    # The fourth field says whether the candidate is a Markdown link target.
    awk -v doc="$doc" '
        {
            rest = $0
            while (match(rest, /((\.\.\/)+|\.\/)?(crates|extensions)\/[A-Za-z0-9_.<>*\/-]+/)) {
                candidate = substr(rest, RSTART, RLENGTH)
                before = RSTART > 1 ? substr(rest, RSTART - 1, 1) : ""
                before2 = RSTART > 2 ? substr(rest, RSTART - 2, 1) : ""
                is_link = before == "(" && before2 == "]" ? 1 : 0
                if (before == "" || before !~ /[A-Za-z0-9_.\/-]/ || is_link) {
                    while (candidate ~ /\.$/) sub(/\.$/, "", candidate)
                    printf "%s|%d|%s|%d\n", doc, NR, candidate, is_link
                }
                rest = substr(rest, RSTART + RLENGTH)
            }
        }
    ' "$doc" >> "$tmp/refs"
done < "$tmp/docs"

[ -s "$tmp/refs" ] || {
    echo "FAIL: no crate/extension paths found; the document parser is broken" >&2
    exit 1
}

while IFS='|' read -r doc line candidate is_link; do
    case "$candidate" in
        *'*'*|*'<'*|*'>'*|crates/CRATE|extensions/wasm/kiqi-)
            continue
            ;;
        crates/kiqi-cli/.ki|crates/kiqi-cli/.ki/*|\
        extensions/conformance/extension.wasm|\
        extensions/wasm/target|extensions/wasm/target/*)
            continue
            ;;
    esac

    # ../ and ./ are explicitly document-relative. A plain path is normally a
    # root-relative source pointer, except when used as a Markdown link target.
    case "$candidate" in
        ../*|./*) target="$ROOT/$(dirname "$doc")/$candidate" ;;
        *)
            if [ "$is_link" -eq 1 ]; then
                target="$ROOT/$(dirname "$doc")/$candidate"
            else
                target="$ROOT/$candidate"
            fi
            ;;
    esac

    if [ ! -e "$target" ]; then
        printf '%s:%s: missing repository path: %s\n' \
            "$doc" "$line" "$candidate" >> "$tmp/missing"
    fi
done < "$tmp/refs"

if [ -s "$tmp/missing" ]; then
    cat "$tmp/missing" >&2
    count=$(wc -l < "$tmp/missing" | tr -d ' ')
    echo "FAIL: $count missing documentation path reference(s)" >&2
    exit 1
fi

docs=$(wc -l < "$tmp/docs" | tr -d ' ')
refs=$(wc -l < "$tmp/refs" | tr -d ' ')
echo "doc paths: $refs references across $docs Markdown files resolve (examples/generated paths exempt)"
