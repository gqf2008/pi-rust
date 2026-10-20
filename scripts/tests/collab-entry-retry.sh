#!/bin/sh
# Exercise the bounded retry wrapper with a fake walgit on PATH.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"

cat > "$tmp/bin/walgit" <<'FAKE'
#!/bin/sh
n=$(cat "$WALGIT_COUNTER" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$WALGIT_COUNTER"
case "${WALGIT_MODE:-transient}" in
    transient)
        if [ "$n" -lt 3 ]; then echo "HTTP 503" >&2; exit 1; fi
        echo '{"ok":true}'
        exit 0
        ;;
    validation)
        echo 'schema error: validation failed' >&2
        exit 1
        ;;
    always)
        echo 'connection reset by peer' >&2
        exit 1
        ;;
esac
FAKE
chmod +x "$tmp/bin/walgit"

run() {
    PATH="$tmp/bin:$PATH" WALGIT_COUNTER="$tmp/counter" WALGIT_MODE="$1" \
        COLLAB_RETRY_DELAY=0 COLLAB_RETRIES=3 \
        sh "$ROOT/scripts/collab-entry-retry.sh" --kind issue --id x --body '{}'
}

rm -f "$tmp/counter"
out=$(run transient)
printf '%s' "$out" | grep -q '"ok":true'
[ "$(cat "$tmp/counter")" -eq 3 ] || { echo "transient attempts != 3" >&2; exit 1; }

rm -f "$tmp/counter"
if run validation >/dev/null 2>&1; then echo "validation error was retried" >&2; exit 1; fi
[ "$(cat "$tmp/counter")" -eq 1 ] || { echo "validation attempted more than once" >&2; exit 1; }

rm -f "$tmp/counter"
if run always >/dev/null 2>&1; then echo "always-failing push succeeded" >&2; exit 1; fi
[ "$(cat "$tmp/counter")" -eq 3 ] || { echo "bounded retry count != 3" >&2; exit 1; }

echo "collab-entry-retry: transient retried, validation not, cap honoured"
