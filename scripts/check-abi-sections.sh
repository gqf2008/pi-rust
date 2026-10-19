#!/bin/sh
# Check the round-by-round section numbers of `docs/design/plugin-abi.md`.
#
# That file is the single source of truth for the wasm contract, and AGENTS.md tells
# contributors to add a §6.x per change. Two failures had gone unnoticed:
#
#   1. §6.34 was cited by four files (AGENTS.md, plugin-abi-handover.md twice,
#      ki-mcp-builtin.md) but had never been written — `git log -S "6.34" -- that
#      file` returns nothing. A reader following the pointer found nothing and could
#      not tell whether the rule had changed or the citation was wrong.
#   2. The sequence had a missing number (6.34) and a duplicated one (§6.36 used
#      for both the goal→orchestrator link and `proc.cancel-requested`), with the
#      duplicate out of order after §6.37.
#
# Checks:
#   A. section numbers are strictly increasing from 6.1 with no gaps or repeats;
#   B. every `§6.N` cited anywhere else in the repo resolves to a real section.
#
# POSIX sh only. Both checks print what is wrong and exit 1.
set -eu

ABI_FILE="docs/design/plugin-abi.md"
[ -f "$ABI_FILE" ] || { echo "missing $ABI_FILE" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

status=0

# --- A. sequence integrity -------------------------------------------------
# The numbers, in file order.
grep -oE '^### 6\.[0-9]+' "$ABI_FILE" | sed 's/^### 6\.//' > "$tmp/seq"

if [ ! -s "$tmp/seq" ]; then
    echo "no §6.N sections found in $ABI_FILE — did the heading format change?" >&2
    exit 1
fi

expected=1
while IFS= read -r n; do
    if [ "$n" -ne "$expected" ]; then
        if [ "$n" -eq $((expected - 1)) ]; then
            echo "$ABI_FILE: duplicate section §6.$n (expected §6.$expected)" >&2
        elif [ "$n" -lt "$expected" ]; then
            echo "$ABI_FILE: §6.$n out of order (expected §6.$expected)" >&2
        else
            echo "$ABI_FILE: missing §6.$expected (jumps to §6.$n)" >&2
        fi
        status=1
    fi
    expected=$((n + 1))
done < "$tmp/seq"

# --- B. citations resolve --------------------------------------------------
# Every §6.N mentioned outside the ABI file itself.
grep -rhoE '§6\.[0-9]+' --include='*.md' --include='*.rs' \
    docs crates plugins AGENTS.md README.md 2>/dev/null \
    | sed 's/^§6\.//' | sort -un > "$tmp/cited"

if [ -s "$tmp/cited" ]; then
    sort -un "$tmp/seq" > "$tmp/have"
    grep -vx -F -f "$tmp/have" "$tmp/cited" > "$tmp/dangling" || true
    if [ -s "$tmp/dangling" ]; then
        echo "citations that point at a section which does not exist:" >&2
        while IFS= read -r n; do
            echo "  §6.$n — cited by:" >&2
            grep -rlE "§6\.$n(\b|[^0-9])" --include='*.md' --include='*.rs' \
                docs crates plugins AGENTS.md README.md 2>/dev/null \
                | sed 's/^/      /' >&2
        done < "$tmp/dangling"
        status=1
    fi
fi

if [ "$status" -eq 0 ]; then
    sections=$(wc -l < "$tmp/seq" | tr -d ' ')
    cites=$(wc -l < "$tmp/cited" | tr -d ' ')
    echo "plugin-abi sections 6.1-6.$sections are contiguous; $cites cited numbers all resolve"
fi

exit "$status"
