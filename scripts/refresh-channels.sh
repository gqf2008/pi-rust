#!/usr/bin/env bash
# Rewrite the install-channel manifests + site data for a released version.
#
#   bash scripts/refresh-channels.sh 0.3.4
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

VERSION="${1:?usage: refresh-channels.sh X.Y.Z}"
REPO="gqf2008/pi-rust"
TAG="v$VERSION"

die() { echo "error: $*" >&2; exit 1; }
note() { echo "» $*"; }

cd "$(git rev-parse --show-toplevel)"

OLD=$(grep -m1 -oE '"version": "[0-9.]+"' packaging/scoop/ki.json | grep -oE '[0-9.]+')
[[ -n "$OLD" ]] || die "could not read the current version from packaging/scoop/ki.json"
[[ "$OLD" != "$VERSION" ]] || { note "manifests already at $VERSION"; exit 0; }

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
  note "  digest unavailable for $asset; falling back to the .sha256 sidecar"
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
mapfile -t OLD_BREW < <(grep -oE 'sha256 "[0-9a-f]+"' packaging/homebrew/ki.rb | grep -oE '[0-9a-f]{64}')
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
DATE=$(date -u +%Y-%m-%d)
OLD_WINGET=$(grep -m1 -oE 'InstallerSha256: [0-9A-Fa-f]+' packaging/winget/gqf2008.ki.installer.yaml | grep -oE '[0-9A-Fa-f]{64}')
[[ -n "$OLD_WINGET" ]] || die "could not read the winget InstallerSha256"
WIN_UPPER=$(printf '%s' "$SHA_WIN_X64" | tr 'a-f' 'A-F')
perl -pi -e "s/PackageVersion: $OLD/PackageVersion: $VERSION/; s/v$OLD/v$VERSION/g; s/$OLD_WINGET/$WIN_UPPER/; s/ReleaseDate: [0-9-]+/ReleaseDate: $DATE/" \
  packaging/winget/gqf2008.ki.installer.yaml
perl -pi -e "s/PackageVersion: $OLD/PackageVersion: $VERSION/; s/v$OLD/v$VERSION/g" \
  packaging/winget/gqf2008.ki.locale.en-US.yaml
perl -pi -e "s/PackageVersion: $OLD/PackageVersion: $VERSION/" \
  packaging/winget/gqf2008.ki.yaml

# --------------------------------------------------------------------------
# Site data: the "current release" card + the version string.
# --------------------------------------------------------------------------
note "website/data/{packages,locales}.json"
# Every "vX.Y.Z · <suffix>" label, not only "· current": the catalog also uses
# "· SDK", "· loader" and "· sessions", which used to keep the old version forever.
perl -pi -e "s/v$OLD · /v$VERSION · /g" website/data/packages.json
perl -pi -e "s/\"version\": \"$OLD\"/\"version\": \"$VERSION\"/" website/data/locales.json

# --------------------------------------------------------------------------
# packaging/README.md: the sha table label/rows + the example tag.
# --------------------------------------------------------------------------
note "packaging/README.md"
perl -pi -e "s/sha256 \(v$OLD\)/sha256 (v$VERSION)/; s/TAG=v$OLD/TAG=v$VERSION/; s/v$OLD/v$VERSION/g" packaging/README.md
mapfile -t OLD_TABLE < <(grep -oE '\| `[0-9a-f]{64}` \|' packaging/README.md | grep -oE '[0-9a-f]{64}')
if [[ "${#OLD_TABLE[@]}" -eq 4 ]]; then
  # Table order: linux x86_64, linux aarch64, apple aarch64, windows.
  perl -pi -e "s/${OLD_TABLE[0]}/$SHA_LINUX_X64/" packaging/README.md
  perl -pi -e "s/${OLD_TABLE[1]}/$SHA_LINUX_ARM/" packaging/README.md
  perl -pi -e "s/${OLD_TABLE[2]}/$SHA_MAC_ARM/" packaging/README.md
  perl -pi -e "s/${OLD_TABLE[3]}/$SHA_WIN_X64/" packaging/README.md
fi

note "done — review the diff, then commit the channel refresh"
