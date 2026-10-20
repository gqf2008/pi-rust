#!/bin/sh
# Assert the version numbers the installers suggest actually exist.
#
# `install.sh --help` is the first thing a user reads, and `install.ps1`'s `.EXAMPLE` plus
# its parse-error hint are the same kind of copy-pasteable instruction. Both used to name
# versions this repository never released:
#
#     install.sh:  --version <v>  e.g. --version 0.3.0      -> 404
#     install.ps1: -Version 0.3.19                          -> 404
#     install.ps1: "pass -Version (for example -Version 0.3.19)"  -> 404
#
# Measured: this repo's releases are v0.4.0 and v0.4.1 only. A suggested-but-absent version
# is worse than no example, because it looks authoritative and fails at download time.
#
# This must ask what was *released*, not what was tagged. A local checkout has tags for
# v0.3.0 and v0.3.19 that were never published here -- so a tag-based check passes on
# exactly the versions that 404 for a user. Online is required; the network is tried
# first and only an explicit KI_CHECK_RELEASES_ALLOW_TAG_FALLBACK=1 permits falling back
# to tags when the API is unreachable (no network at all, e.g. a locked-down laptop).
# That fallback is weaker and can be satisfied by the very bug this catches, so the
# script prints a warning when it takes it, and refuses to be silent about it.
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SH="$ROOT/scripts/install.sh"
PS1="$ROOT/scripts/install.ps1"

for f in "$SH" "$PS1"; do
    [ -f "$f" ] || { echo "FAIL: $f not found" >&2; exit 1; }
done

# `x.y.z`, not `x.y` — a two-part version would match prose like "0.4".
semver_re='(^|[^0-9.])([0-9]+\.[0-9]+\.[0-9]+)([^0-9.]|$)'

# Lines that suggest a version to the user, in both installers:
#   sh : "--version <v>  Install a specific version, e.g. --version 0.4.1"
#   ps1: ".EXAMPLE & .\install.ps1 -Version 0.4.1 ..."  and the -Version hint in a throw
suggestions() {
    # shellcheck disable=SC2016
    # NOTE: do *not* filter out `#` lines here. `install.sh --help` is literally
    # `sed -n '2,16p' "$0"`, so its user-facing option text lives in comments -- excluding
    # them skipped the one suggestion the user is most likely to copy.
    grep -nE -- '--version [0-9]|-Version [0-9]|e\.g\. --version' "$SH" "$PS1" 2>/dev/null \
        | sed 's/^/  /'
}

status=0
found=0

while IFS= read -r line; do
    [ -n "$line" ] || continue
    file=${line%%:*}
    found=$((found + 1))
    echo "$line" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | while IFS= read -r ver; do
        echo "  suggested: $ver  (${file#"$ROOT"/})"
    done
done <<EOF
$(suggestions)
EOF

# Collect suggested versions, then check each against the tags.
versions=$(suggestions | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | sort -u)

if [ -z "$versions" ]; then
    echo "FAIL: no version suggestion found in install.sh or install.ps1" >&2
    echo "      (the parser probably broke, not the installers)" >&2
    exit 1
fi

online=$(curl -fsSL --max-time 25 \
    "https://api.github.com/repos/gqf2008/pi-rust/releases?per_page=100" 2>/dev/null \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\([^"]*\)".*/\1/p' | sort -u)

if [ -n "$online" ]; then
    released="$online"
else
    if [ "${KI_CHECK_RELEASES_ALLOW_TAG_FALLBACK:-0}" = "1" ]; then
        released=$(cd "$ROOT" && git tag --list 'v*' 2>/dev/null | sed 's/^v//' | sort -u)
        echo "warning: GitHub unreachable; checking local tags, which include versions that" >&2
        echo "      were never published (that is the failure this check exists to catch)" >&2
    else
        echo "FAIL: could not read the release list from GitHub" >&2
        echo "      set KI_CHECK_RELEASES_ALLOW_TAG_FALLBACK=1 to fall back to local tags (weaker)" >&2
        exit 1
    fi
fi

for ver in $versions; do
    if printf '%s\n' "$released" | grep -qxF "$ver"; then
        echo "  ok: $ver is a real release"
    else
        echo "FAIL: the installers suggest $ver, which is not a released version" >&2
        echo "      a user who copies it gets a 404 at download time" >&2
        echo "      released: $(printf '%s' "$released" | tr '\n' ' ')" >&2
        status=1
    fi
done

if [ "$status" -eq 0 ]; then
    echo "installers only suggest versions that exist"
fi
exit "$status"
