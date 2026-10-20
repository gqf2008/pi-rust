#!/usr/bin/env bash
# Retry a transient `walgit collab entry` push.
#
#   scripts/collab-entry-retry.sh --kind issue --id ... --body ...
#
# Network/5xx failures are retried a bounded number of times. Schema,
# validation and signature errors fail on the first attempt: a bad entry must
# not be silently retried into a different failure, and an append-only thread
# should not receive duplicate entries from a retry loop that cannot tell.
set -uo pipefail

retries="${COLLAB_RETRIES:-3}"
delay="${COLLAB_RETRY_DELAY:-1}"
case "$retries" in
    ''|*[!0-9]*) echo "COLLAB_RETRIES must be a number" >&2; exit 2 ;;
esac
[ "$retries" -ge 1 ] || retries=1

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

transient() {
    printf '%s' "$1" | grep -Eqi \
        'HTTP[ /]?5[0-9][0-9]|status[^0-9]*5[0-9][0-9]|502|503|504|connection (reset|refused)|timed out|timeout|temporarily unavailable|network|EOF|failed to push|could not resolve'
}

validation() {
    printf '%s' "$1" | grep -Eqi \
        'schema|validation|signature|invalid|unknown action|no entries for thread|already exists|bad request|400'
}

attempt=1
while :; do
    out="$work/out"
    err="$work/err"
    walgit collab entry "$@" >"$out" 2>"$err"
    status=$?
    if [ "$status" -eq 0 ]; then
        cat "$out"
        exit 0
    fi
    combined="$(cat "$out" "$err" 2>/dev/null)"
    if [ "$attempt" -ge "$retries" ] || validation "$combined" || ! transient "$combined"; then
        cat "$out" 2>/dev/null
        cat "$err" >&2
        exit "$status"
    fi
    echo "collab entry attempt $attempt/$retries failed transiently; retrying in ${delay}s" >&2
    [ "$delay" -gt 0 ] && sleep "$delay"
    delay=$((delay * 2))
    attempt=$((attempt + 1))
done
