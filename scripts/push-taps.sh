#!/usr/bin/env bash
# Push the refreshed Homebrew formula + Scoop manifest into their tap repos.
#
#   bash scripts/push-taps.sh 0.3.4
#
# The files in packaging/ are the source of truth; the tap repos are copies.
# Uses the GitHub contents API (needs write access to both repos).

set -euo pipefail

VERSION="${1:?usage: push-taps.sh X.Y.Z}"
MSG="ki $VERSION"
# The taps belong to this project's owner. bigfish1913/* is an unrelated fork and
# its tap/bucket carry a different tool (`rpi`); never push there.
TAP_OWNER="${KI_TAP_OWNER:-gqf2008}"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "» $*"; }

cd "$(git rev-parse --show-toplevel)"

# put REPO PATH LOCAL_FILE — create or update a file via the contents API.
put() {
  local repo="$1" path="$2" file="$3"
  [[ -f "$file" ]] || die "missing $file"
  local content
  content=$(base64 <"$file" | tr -d '\n')
  local sha
  sha=$(gh api "repos/$repo/contents/$path" --jq .sha 2>/dev/null || true)
  if [[ -n "$sha" ]]; then
    note "update $repo/$path"
    gh api --method PUT "repos/$repo/contents/$path" \
      -f message="$MSG" -f content="$content" -f sha="$sha" >/dev/null
  else
    note "create $repo/$path"
    gh api --method PUT "repos/$repo/contents/$path" \
      -f message="$MSG" -f content="$content" >/dev/null
  fi
}

for repo in "$TAP_OWNER/homebrew-tap" "$TAP_OWNER/scoop-bucket"; do
  gh api "repos/$repo" >/dev/null 2>&1 \
    || die "$repo does not exist; create it first (or set KI_TAP_OWNER). Refusing to push."
done

put "$TAP_OWNER/homebrew-tap" "Formula/ki.rb" "packaging/homebrew/ki.rb"
put "$TAP_OWNER/scoop-bucket" "bucket/ki.json" "packaging/scoop/ki.json"

note "taps pushed"
