#!/usr/bin/env bash
# Rewrite the install-channel manifests + site data for a released version.
#
#   bash scripts/refresh-channels.sh 0.3.4
#   FORCE=1 bash scripts/refresh-channels.sh 0.3.4   # after re-uploading a tag's assets
#
# Requires the release assets to exist (run after the binaries workflow
# finishes). It reads the asset digests from the GitHub API and does targeted
# substitutions — no templates — so a format tweak upstream does not silently
# drop. Idempotent: re-running against the same version is a no-op.
#
# Why the API digest and not the `.sha256` sidecars: this machine cannot reach
# `release-assets.githubusercontent.com` (the host every release-asset URL
# redirects to), so both `gh release download` and a plain `curl -L` time out.
# The API's `digest` field is the sha256 GitHub computed for the uploaded asset
# — the same value the sidecar contains — so it removes the download entirely
# and is reachable. Verified against v0.3.9: every API digest matched the hash
# already recorded in the Homebrew/Scoop manifests.

set -euo pipefail

VERSION="${1:?usage: refresh-channels.sh X.Y.Z [YYYY-MM-DD]}"
# The release date is a fact about the release, not about when this script happens
# to run. release.sh passes its --date through; a bare invocation falls back to
# today (UTC), which matches the old behaviour.
RELEASE_DATE="${2:-}"
REPO="gqf2008/pi-rust"
TAG="v$VERSION"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "» $*"; }

cd "$(git rev-parse --show-toplevel)"

# The version every manifest currently names. Keying the "already refreshed"
# gate on Scoop alone made a half-applied refresh look complete: the script
# rewrites Scoop before winget and the README table, so an interruption left
# those two stale while a re-run printed "already at X" and exited 0.
version_of() {
  case "$1" in
    homebrew) grep -m1 -oE 'version "[0-9.]+"' packaging/homebrew/ki.rb | grep -oE '[0-9.]+' ;;
    scoop) grep -m1 -oE '"version": "[0-9.]+"' packaging/scoop/ki.json | grep -oE '[0-9.]+' ;;
    winget) grep -m1 -oE 'PackageVersion: [0-9.]+' packaging/winget/gqf2008.ki.installer.yaml | grep -oE '[0-9.]+' ;;
    readme) grep -m1 -oE 'sha256 \(v[0-9.]+\)' packaging/README.md | sed -E 's/.*\(v//; s/\)$//' ;;
    *) die "unknown manifest: $1" ;;
  esac
}
OLD=$(version_of scoop || true)
[[ -n "$OLD" ]] || die "could not read the current version from packaging/scoop/ki.json"
HB_NOW=$(version_of homebrew || true)
WG_NOW=$(version_of winget || true)
RD_NOW=$(version_of readme || true)
# A re-cut release re-uploads the same tag's assets, so the version is already
# current while the digests are stale. FORCE=1 refreshes without a bump.
if [[ "${FORCE:-0}" != "1" ]] \
  && [[ "$HB_NOW" == "$VERSION" && "$OLD" == "$VERSION" \
     && "$WG_NOW" == "$VERSION" && "$RD_NOW" == "$VERSION" ]]; then
  note "manifests already at $VERSION"
  exit 0
fi
if [[ "$HB_NOW" != "$OLD" || "$OLD" != "$WG_NOW" || "$WG_NOW" != "$RD_NOW" ]]; then
  note "  manifests disagree before refresh (homebrew=${HB_NOW:-?} scoop=${OLD:-?} winget=${WG_NOW:-?} readme=${RD_NOW:-?}); refreshing all"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# sha ASSET -> lowercase hex digest for the asset named `$1`.
#
# Prefers the API's `digest`; falls back to downloading the `.sha256` sidecar
# for a GitHub instance that does not populate `digest` yet. The sidecar content
# is "<hex>  <filename>", hence the awk.
sha() {
  local asset="$1"
  local digest
  digest=$(gh api "repos/$REPO/releases/tags/$TAG" \
    --jq ".assets[] | select(.name == \"$asset\") | .digest" 2>/dev/null \
    | sed 's/^sha256://' | tr 'A-F' 'a-f')
  if [[ "$digest" =~ ^[0-9a-f]{64}$ ]]; then
    printf '%s\n' "$digest"
    return 0
  fi
  # stderr, not stdout: this function's stdout is the captured digest, and a
  # note on stdout ended up inside the Homebrew formula's sha256 string in
  # the 0.4.6 release.
  note "  digest unavailable for $asset; falling back to the .sha256 sidecar" >&2
  gh release download "$TAG" -R "$REPO" -p "$asset.sha256" -D "$tmp" >/dev/null \
    || die "could not fetch $asset.sha256"
  local file="$tmp/$asset.sha256"
  [[ -f "$file" ]] || die "missing $asset.sha256 in the $TAG release"
  awk '{print $1}' "$file" | tr 'A-F' 'a-f'
}

# Asset names come from `release-binaries.yml`: `name="ki-v${version}-${target}"`,
# i.e. the `ki` command name and the version *without* its `v` prefix. These used to
# read `kiqi-$TAG-…` (old binary name + the prefixed tag), which matches no uploaded
# asset — every digest lookup would fall back to the sidecar and fail.
LINUX_X64="ki-v$VERSION-x86_64-unknown-linux-gnu.tar.gz"
LINUX_ARM="ki-v$VERSION-aarch64-unknown-linux-gnu.tar.gz"
MAC_ARM="ki-v$VERSION-aarch64-apple-darwin.tar.gz"
WIN_X64="ki-v$VERSION-x86_64-pc-windows-msvc.zip"

SHA_LINUX_X64=$(sha "$LINUX_X64")
SHA_LINUX_ARM=$(sha "$LINUX_ARM")
SHA_MAC_ARM=$(sha "$MAC_ARM")
SHA_WIN_X64=$(sha "$WIN_X64")

# --------------------------------------------------------------------------
# Homebrew: version + URLs + the three sha256 lines (order in file:
# apple-darwin, linux aarch64, linux x86_64).
# --------------------------------------------------------------------------
note "packaging/homebrew/ki.rb"
# macOS still ships Bash 3.2, which has arrays but no `mapfile`.
OLD_BREW=()
while IFS= read -r hash; do
  OLD_BREW+=("$hash")
done < <(grep -oE 'sha256 "[0-9a-f]+"' packaging/homebrew/ki.rb | grep -oE '[0-9a-f]{64}')
[[ "${#OLD_BREW[@]}" -eq 3 ]] || die "expected 3 sha256 lines in the Homebrew formula"
perl -pi -e "s/version \"$OLD\"/version \"$VERSION\"/; s/v$OLD/v$VERSION/g" packaging/homebrew/ki.rb
perl -pi -e "s/${OLD_BREW[0]}/$SHA_MAC_ARM/" packaging/homebrew/ki.rb
perl -pi -e "s/${OLD_BREW[1]}/$SHA_LINUX_ARM/" packaging/homebrew/ki.rb
perl -pi -e "s/${OLD_BREW[2]}/$SHA_LINUX_X64/" packaging/homebrew/ki.rb

# --------------------------------------------------------------------------
# Scoop
# --------------------------------------------------------------------------
note "packaging/scoop/ki.json"
OLD_SCOOP=$(grep -m1 -oE '"hash": "[0-9a-fA-F]+"' packaging/scoop/ki.json | grep -oE '[0-9a-fA-F]{64}')
[[ -n "$OLD_SCOOP" ]] || die "could not read the Scoop hash"
perl -pi -e "s/\"version\": \"$OLD\"/\"version\": \"$VERSION\"/; s/v$OLD/v$VERSION/g; s/$OLD_SCOOP/$SHA_WIN_X64/" packaging/scoop/ki.json

# --------------------------------------------------------------------------
# winget
# --------------------------------------------------------------------------
note "packaging/winget/*.yaml"
DATE="${RELEASE_DATE:-$(date -u +%Y-%m-%d)}"
OLD_WINGET=$(grep -m1 -oE 'InstallerSha256: [0-9A-Fa-f]+' packaging/winget/gqf2008.ki.installer.yaml | grep -oE '[0-9A-Fa-f]{64}')
[[ -n "$OLD_WINGET" ]] || die "could not read the winget InstallerSha256"
WIN_UPPER=$(printf '%s' "$SHA_WIN_X64" | tr 'a-f' 'A-F')
# The manifest stores the date quoted (winget's schema wants a string, not a YAML
# timestamp); the pattern must tolerate the quotes or the substitution silently
# no-ops, which is how 0.4.6-0.5.3 all kept the 2026-10-28 date.
perl -pi -e "s/PackageVersion: $OLD/PackageVersion: $VERSION/; s/v$OLD/v$VERSION/g; s/$OLD_WINGET/$WIN_UPPER/; s/ReleaseDate: \"?[0-9-]+\"?/ReleaseDate: \"$DATE\"/" \
  packaging/winget/gqf2008.ki.installer.yaml
perl -pi -e "s/PackageVersion: $OLD/PackageVersion: $VERSION/; s/v$OLD/v$VERSION/g" \
  packaging/winget/gqf2008.ki.locale.en-US.yaml
perl -pi -e "s/PackageVersion: $OLD/PackageVersion: $VERSION/" \
  packaging/winget/gqf2008.ki.yaml

# --------------------------------------------------------------------------
# packaging/README.md: the sha table label/rows + the example tag.
# --------------------------------------------------------------------------
note "packaging/README.md"
# The table carries its own version token, and after a re-cut (`FORCE=1`) the manifests
# are already at $VERSION while the table still names the previous one -- a substitution
# keyed on $OLD then matches nothing and the stale label/asset names survive. Key on the
# label the file actually has. The asset names also lost their `kiqi-` prefix in 0.4.0,
# so a row that still says `kiqi-v...` is stale even at the right version.
README_OLD=$(grep -m1 -oE 'sha256 \(v[0-9.]+\)' packaging/README.md | sed -E 's/.*\(v//; s/\)$//' || true)
if [[ -n "$README_OLD" ]]; then
  perl -pi -e "s/sha256 \(v$README_OLD/sha256 (v$VERSION/; s/TAG=v$README_OLD/TAG=v$VERSION/g; s/v$README_OLD-/v$VERSION-/g" packaging/README.md
fi
perl -pi -e "s/kiqi-v$VERSION-/ki-v$VERSION-/g" packaging/README.md
# Rows are keyed by their Target column, not by position. The old code collected
# "rows with a hash" and mapped them by ordinal, so adding the Intel-macOS row (or
# reordering the table) shipped stale or misassigned digests with exit 0.
# A target the script does not know is a hard error in the verification step below.
SHA_MAC_X64=""
if grep -q 'x86_64-apple-darwin' packaging/README.md; then
  SHA_MAC_X64=$(sha "ki-v$VERSION-x86_64-apple-darwin.tar.gz")
fi
KI_SHA_LINUX_X64=$SHA_LINUX_X64 KI_SHA_LINUX_ARM=$SHA_LINUX_ARM \
KI_SHA_MAC_ARM=$SHA_MAC_ARM KI_SHA_MAC_X64=$SHA_MAC_X64 KI_SHA_WIN=$SHA_WIN_X64 \
  perl -pi -e '
    BEGIN {
      %sha = (
        "x86_64-unknown-linux-gnu"  => $ENV{KI_SHA_LINUX_X64},
        "aarch64-unknown-linux-gnu" => $ENV{KI_SHA_LINUX_ARM},
        "aarch64-apple-darwin"      => $ENV{KI_SHA_MAC_ARM},
        "x86_64-apple-darwin"       => $ENV{KI_SHA_MAC_X64},
        "x86_64-pc-windows-msvc"    => $ENV{KI_SHA_WIN},
      );
    }
    if (/^\| `([a-z0-9_-]+)` \| `ki-v[0-9.]+-[a-z0-9_-]+\.(?:tar\.gz|zip)` \| `[0-9a-f]{64}` \|$/) {
      my $target = $1;
      if (defined $sha{$target} && $sha{$target} ne "") {
        s/\| `[0-9a-f]{64}` \|$/| `$sha{$target}` |/;
      }
    }
  ' packaging/README.md

# Verify the whole refresh instead of trusting the substitutions: every manifest
# names $VERSION and every table row carries the digest fetched for its target.
# A silent no-op (format drift, a new target) now fails here instead of shipping.
note "verify"
for pair in "homebrew:$(version_of homebrew || true)" "scoop:$(version_of scoop || true)" \
            "winget:$(version_of winget || true)" "readme:$(version_of readme || true)"; do
  name=${pair%%:*}
  got=${pair#*:}
  [[ "$got" == "$VERSION" ]] || die "$name is at ${got:-?}, expected $VERSION"
done
perl -ne 'if (/^\| `([a-z0-9_-]+)` \| `ki-v[0-9.]+-[a-z0-9_-]+\.(?:tar\.gz|zip)` \| `([0-9a-f]{64})` \|$/) { print "$1 $2\n" }' \
  packaging/README.md > "$tmp/readme_rows"
[[ -s "$tmp/readme_rows" ]] || die "no sha256 rows parsed from packaging/README.md"
while read -r target digest; do
  case "$target" in
    x86_64-unknown-linux-gnu) want=$SHA_LINUX_X64 ;;
    aarch64-unknown-linux-gnu) want=$SHA_LINUX_ARM ;;
    aarch64-apple-darwin) want=$SHA_MAC_ARM ;;
    x86_64-apple-darwin) want=$SHA_MAC_X64 ;;
    x86_64-pc-windows-msvc) want=$SHA_WIN_X64 ;;
    *) die "unknown target in the packaging/README sha table: $target" ;;
  esac
  [[ "$digest" == "$want" ]] || die "sha table row $target has $digest, expected $want"
done < "$tmp/readme_rows"

note "done — review the diff, then commit the channel refresh"
