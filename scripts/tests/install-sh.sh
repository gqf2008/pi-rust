#!/bin/sh
# Exercise `scripts/install.sh` without a network or a real download.
#
# `install.sh` is the `curl -fsSL … | sh` entry point — the first thing a user
# runs — and until this script existed nothing tested it. `install.ps1` has
# `scripts/tests/install.Tests.ps1`, but that runs only in the Windows job, so the
# Unix half had no coverage in any CI job.
#
# The script has a real seam for this: `--dry-run --version <v>` prints the detected
# platform, the version and the download URL, then exits without touching the network.
# Platform detection calls `uname`, which a PATH shim can answer — so every branch the
# release workflow actually builds can be exercised here, on any machine.
#
# What this pins:
#   * each supported (os, arch) pair maps to the target triple the release job builds;
#   * the asset name follows the platform (`zip` on Windows, `tar.gz` elsewhere);
#   * an unsupported platform fails with a message that names the platform and points
#     at building from source, rather than downloading something that cannot exist.
#
# It deliberately does not test the download/extract path: that needs the network and a
# real release, which belongs in a live check, not the default gate.
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
INSTALL="$ROOT/scripts/install.sh"
VERSION=0.4.1

[ -f "$INSTALL" ] || { echo "missing $INSTALL" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# A `uname` that answers from the environment, so each platform branch can be driven
# without cross-compiling or being on that platform.
mkdir -p "$tmp/bin"
cat > "$tmp/bin/uname" <<'SHIM'
#!/bin/sh
case "$1" in
  -m) echo "${FAKE_ARCH:?}" ;;
  *)  echo "${FAKE_OS:?}" ;;
esac
SHIM
chmod +x "$tmp/bin/uname"

status=0

check_platform() {
    os=$1
    arch=$2
    want=$3
    want_asset_suffix=$4
    got=$(PATH="$tmp/bin:$PATH" FAKE_OS="$os" FAKE_ARCH="$arch" \
        sh "$INSTALL" --dry-run --version "$VERSION" 2>&1) || {
        echo "FAIL: $os/$arch exited non-zero: $got" >&2
        status=1
        return
    }
    echo "$got" | grep -qx "platform: $want" || {
        echo "FAIL: $os/$arch → expected 'platform: $want', got: $got" >&2
        status=1
        return
    }
    echo "$got" | grep -q "\.$want_asset_suffix\$" || {
        echo "FAIL: $os/$arch → expected a .$want_asset_suffix asset, got: $got" >&2
        status=1
        return
    }
    echo "ok: $os/$arch → $want (.$want_asset_suffix)"
}

# Every triple the release workflow builds (`release-binaries.yml`).
check_platform Linux    x86_64  x86_64-unknown-linux-gnu   tar.gz
check_platform Linux    aarch64 aarch64-unknown-linux-gnu  tar.gz
check_platform Darwin   arm64   aarch64-apple-darwin       tar.gz
check_platform Darwin   x86_64  x86_64-apple-darwin        tar.gz
check_platform MINGW64_NT x86_64 x86_64-pc-windows-msvc    zip

# Unsupported platforms must fail loudly and point at the source build.
for bad in "FreeBSD x86_64" "Linux riscv64"; do
    set -- $bad
    got=$(PATH="$tmp/bin:$PATH" FAKE_OS="$1" FAKE_ARCH="$2" \
        sh "$INSTALL" --dry-run --version "$VERSION" 2>&1) && {
        echo "FAIL: $1/$2 should have been rejected, got: $got" >&2
        status=1
        continue
    }
    echo "$got" | grep -q "cargo build --release" || {
        echo "FAIL: $1/$2 rejection must point at a source build, got: $got" >&2
        status=1
        continue
    }
    echo "ok: $1/$2 rejected with a source-build hint"
done

if [ "$status" -eq 0 ]; then
    echo "install.sh: platform detection and asset naming verified"
fi
exit "$status"
