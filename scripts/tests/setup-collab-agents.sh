#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
out=$(sh "$ROOT/scripts/setup-collab-agents.sh" 2 --dry-run)
printf '%s\n' "$out" | grep -q "worktree add .*/collab-agent-1 -b collab/agent-1 main"
printf '%s\n' "$out" | grep -q "walgit collab join --principal ki-agent-2 --push walgit"
printf '%s\n' "$out" | grep -q "launch: cd .*/collab-agent-2 && ki -a"
if sh "$ROOT/scripts/setup-collab-agents.sh" nope --dry-run >/dev/null 2>&1; then
    echo "bad count was accepted" >&2; exit 1
fi
echo "setup-collab-agents: dry-run lists worktrees, principals and launch commands"
