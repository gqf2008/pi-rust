#!/bin/sh
# Prepare per-agent collaboration worktrees: one worktree + one walgit
# principal per agent, so entries are attributable and claim/session state is
# isolated. Idempotent; never deletes anything.
#
#   sh scripts/setup-collab-agents.sh [count] [--dry-run]
#   cd .claude/worktrees/collab-agent-1 && ki -a
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
count=3
dry=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry=1 ;;
        ''|*[!0-9]*) echo "usage: $0 [count] [--dry-run]" >&2; exit 2 ;;
        *) count=$arg ;;
    esac
done
[ "$count" -ge 1 ] || { echo "count must be >= 1" >&2; exit 2; }

command -v git >/dev/null || { echo "git is required" >&2; exit 1; }
# `--dry-run` only prints the commands, so it must not demand the walgit CLI: CI (which
# checks the printed plan) has no walgit, and a plan is exactly what you want before
# installing anything.
if [ "$dry" -ne 1 ]; then
    command -v walgit >/dev/null || { echo "walgit is required" >&2; exit 1; }
fi

i=1
while [ "$i" -le "$count" ]; do
    wt="$ROOT/.claude/worktrees/collab-agent-$i"
    branch="collab/agent-$i"
    principal="ki-agent-$i"
    if [ -e "$wt" ]; then
        echo "skip: $wt already exists"
    elif [ "$dry" -eq 1 ]; then
        echo "git -C $ROOT worktree add $wt -b $branch main"
        echo "(cd $wt && walgit collab join --principal $principal --push walgit)"
    else
        git -C "$ROOT" worktree add "$wt" -b "$branch" main
        (cd "$wt" && walgit collab join --principal "$principal" --push walgit)
    fi
    echo "launch: cd $wt && ki -a"
    i=$((i + 1))
done
