#!/bin/sh
# Assert the version numbers the installers suggest actually exist.
#
# These are the version numbers shown as examples to a human: `install.sh --help` is the
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
# The README's Windows quick-start shows `-Version <x>`, which is just as
# copy-pasteable as the installers' own help text (it had drifted to 0.3.21,
# a tag this repository never released).
README="$ROOT/README.md"
# Workflow inputs are read by a human too -- `release-binaries.yml`'s `tag` description
# told the maintainer to pass "e.g. v0.3.0", which this repository never released.
RELEASE_WF="$ROOT/.github/workflows/release-binaries.yml"

for f in "$SH" "$PS1" "$README" "$RELEASE_WF"; do
    [ -f "$f" ] || { echo "FAIL: $f not found" >&2; exit 1; }
done

# Lines that suggest a version to the user, in both installers:
#   sh : "--version <v>  Install a specific version, e.g. --version 0.4.1"
#   ps1: ".EXAMPLE & .\install.ps1 -Version 0.4.1 ..."  and the -Version hint in a throw
#   md : "Append -DryRun to preview, or use -Version 0.4.1 ..."
suggestions() {
    # shellcheck disable=SC2016
    # NOTE: do *not* filter out `#` lines here. `install.sh --help` is literally
    # `sed -n '2,16p' "$0"`, so its user-facing option text lives in comments -- excluding
    # them skipped the one suggestion the user is most likely to copy.
    grep -HnE -- '--version [0-9]|-Version [0-9]|e\.g\. --version' "$SH" "$PS1" "$README" 2>/dev/null \
        | sed 's/^/  /'
    # `description: "... (e.g. v0.4.1)"` in the release workflow's dispatch input.
    grep -HnE '^[[:space:]]*description:[[:space:]]*"[^"]*\(e\.g\. v[0-9]' "$RELEASE_WF" 2>/dev/null \
        | sed 's/^/  /'
}

status=0
found=0
all=$(suggestions)

while IFS= read -r line; do
    [ -n "$line" ] || continue
    # `grep -n` emits `path:line:text`, but the file path itself may contain `:` on some
    # systems, so strip the trailing `:line:` instead of splitting on the first colon.
    file=$(printf '%s' "$line" | sed -E 's/^[[:space:]]*//; s/:[0-9]+:.*$//')
    found=$((found + 1))
    printf '%s\n' "$line" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | while IFS= read -r ver; do
        echo "  suggested: $ver  (${file#"$ROOT"/})"
    done
done <<EOF
$all
EOF

# Collect suggested versions, then check each against the release list.
versions=$(printf '%s\n' "$all" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | sort -u)

if [ -z "$versions" ]; then
    echo "FAIL: no version suggestion found in install.sh, install.ps1 or README.md" >&2
    echo "      (the parser probably broke, not the installers)" >&2
    exit 1
fi

# Use the workflow token when CI provides one: an unauthenticated request is
# limited to 60/hour per IP, and a rate-limited runner would turn this gate
# red for a reason that has nothing to do with the commit.
if [ -n "${GITHUB_TOKEN:-}" ]; then
    online=$(curl -fsSL --max-time 25 \
        -H "Authorization: Bearer ${GITHUB_TOKEN}" \
        "https://api.github.com/repos/gqf2008/pi-rust/releases?per_page=100" 2>/dev/null \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\([^"]*\)".*/\1/p' | sort -u)
else
    online=$(curl -fsSL --max-time 25 \
        "https://api.github.com/repos/gqf2008/pi-rust/releases?per_page=100" 2>/dev/null \
        | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"v\([^"]*\)".*/\1/p' | sort -u)
fi

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
        echo "FAIL: $ver is suggested as an example, but it is not a released version" >&2
        echo "      anyone who copies it gets a 404 at download time" >&2
        echo "      released: $(printf '%s' "$released" | tr '\n' ' ')" >&2
        status=1
    fi
done

if [ "$status" -eq 0 ]; then
    echo "every suggested example version exists as a release"
fi
exit "$status"
