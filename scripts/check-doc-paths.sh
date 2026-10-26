#!/bin/sh
# Check concrete crate/extension repository paths cited by current documentation.
#
# Scope: the root README/CLAUDE/AGENTS files, docs/**/*.md, and the embedded
# documentation snapshot under crates/kiqi-cli/embedded-docs (which `ki docs`
# serves). CHANGELOG files are skipped, as are documents that describe
# themselves as historical records: a leading "历史存档:" / "historical archive:"
# note, or a "本文…历史存档/历史契约/设计存档" self-description. A retirement
# note that only scopes named paths ("文中 `X` … 是历史记录") exempts just the
# backticked globs before that marker; the rest of the document is still
# checked. Merely citing an archived document does not exempt the citing file.
#
# Only paths which must exist are checked:
#   * wildcards (`extensions/wasm/kiqi-*`) and placeholders (`extensions/<id>`,
#     `crates/CRATE`, or the `extensions/wasm/kiqi-` name prefix) are examples;
#   * `crates/kiqi-cli/.ki`, `extensions/conformance/extension.wasm`, and
#     `extensions/wasm/target` are runtime/test/build output, not source files;
#   * paths below another directory (`.ki/extensions/...`, `docs/extensions/...`,
#     `abb/crates/...`) and crates.io/shields URLs are not root-relative claims.
# Markdown link destinations are resolved from the document's directory, falling
# back to the repository root when the destination has no `./` or `../` prefix:
# the embedded copies and the root-style READMEs link repo-root-relative. Paths
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
: > "$tmp/hist_globs"

for doc in README.md CLAUDE.md AGENTS.md; do
    [ ! -f "$doc" ] || printf '%s\n' "$doc" >> "$tmp/docs"
done
find docs -type f -name '*.md' -print | sort >> "$tmp/docs"
if [ -d crates/kiqi-cli/embedded-docs ]; then
    find crates/kiqi-cli/embedded-docs -type f -name '*.md' -print | sort >> "$tmp/docs"
fi

while IFS= read -r doc; do
    case "$doc" in
        CHANGELOG.md|*/CHANGELOG.md|changelog.md|*/changelog.md) continue ;;
    esac

    # A document that describes itself as an archive is historical throughout.
    if grep -qiE '^>[[:space:]]*(历史存档：|historical archive:)|本文.*(历史存档|历史契约|设计存档)' "$doc"; then
        continue
    fi

    # A retirement note may scope only the paths it names. Record the backticked
    # globs before "历史记录" as exemptions; anything after that marker describes
    # the current layout and stays checked.
    grep -E '文中.*历史记录' "$doc" 2>/dev/null \
        | sed 's/历史记录.*//' \
        | grep -oE '`[^`]*[*][^`]*`' 2>/dev/null \
        | tr -d '`' \
        | while IFS= read -r glob; do
            case "$glob" in
                crates/*|extensions/*)
                    printf '%s|%s\n' "$doc" "$glob" >> "$tmp/hist_globs"
                    ;;
            esac
        done

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

# Test a candidate against the historical globs recorded for its document.
is_scoped_historical() {
    [ -s "$tmp/hist_globs" ] || return 1
    while IFS='|' read -r glob_doc glob; do
        [ "$glob_doc" = "$1" ] || continue
        # Unquoted on purpose: the recorded glob is a case pattern.
        case "$2" in
            $glob) return 0 ;;
        esac
    done < "$tmp/hist_globs"
    return 1
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
    if is_scoped_historical "$doc" "$candidate"; then
        continue
    fi

    # ../ and ./ are explicitly document-relative. A plain path is normally a
    # root-relative source pointer, except when used as a Markdown link target;
    # there, prefer the document-relative reading and fall back to the root.
    case "$candidate" in
        ../*|./*) target="$ROOT/$(dirname "$doc")/$candidate" ;;
        *)
            if [ "$is_link" -eq 1 ]; then
                target="$ROOT/$(dirname "$doc")/$candidate"
                if [ ! -e "$target" ] && [ -e "$ROOT/$candidate" ]; then
                    target="$ROOT/$candidate"
                fi
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
