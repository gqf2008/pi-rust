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
# It also covers the download/extract/verify path, which the first version of this file
# said "needs the network and a real release". It does not: `download()` is
# `curl -fsSL -o <dest> <url>`, so a `curl` shim serving local bytes exercises the real
# code. That turned up a genuine defect -- on a stock macOS PATH there is no `sha256sum`
# (it is a coreutils tool), so the old `[ -f x ] && command -v sha256sum && sha256sum -c`
# form downloaded the published checksum and then discarded it silently, installing
# tampered bytes without a word. `install.ps1` verifies unconditionally; the Unix half
# now tries sha256sum, shasum, openssl, and refuses to install when a checksum exists but
# none of them is usable.
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
# --- download / extract / verify path ----------------------------------------
# The original header of this file said the download path "needs the network and a real
# release". It does not: `download()` is `curl -fsSL -o <dest> <url>`, so a PATH shim can
# serve local bytes. That matters because the verification branch had a real defect —
# on a stock macOS PATH (`sha256sum` absent, `shasum` present) it matched neither tool and
# silently skipped the check after downloading the file, discarding it without a word.

fixtures="$tmp/fixtures"
mkdir -p "$fixtures" "$tmp/bin"

mkdir -p "$tmp/payload"
cp "$ROOT/LICENSE" "$tmp/payload/LICENSE" 2>/dev/null || printf 'license\n' > "$tmp/payload/LICENSE"
cat > "$tmp/payload/ki" <<'FAKEKI'
#!/bin/sh
echo "ki-fake 0.0.0"
FAKEKI
chmod +x "$tmp/payload/ki"
asset="ki-v${VERSION}-aarch64-apple-darwin.tar.gz"
tar -czf "$fixtures/$asset" -C "$tmp/payload" .
(cd "$fixtures" && sha256sum "$asset" > "$asset.sha256" 2>/dev/null || shasum -a 256 "$asset" > "$asset.sha256")
cp "$fixtures/$asset" "$tmp/pristine.tar.gz"

cat > "$tmp/bin/curl" <<'SHIM'
#!/bin/sh
# Serve from $FAKE_FIXTURES, mirroring `curl -fsSL -o <dest> <url>`.
dest=""; url=""
while [ $# -gt 0 ]; do
    case "$1" in
        -o) dest=$2; shift 2 ;;
        -*) shift ;;
        *) url=$1; shift ;;
    esac
done
src="$FAKE_FIXTURES/${url##*/}"
[ -f "$src" ] || exit 22
cp "$src" "$dest"
SHIM
chmod +x "$tmp/bin/curl"

# `shasum` only (no `sha256sum`): the stock macOS situation. Verification must happen.
rm -rf "$tmp/out"
if out=$(PATH="$tmp/bin:/usr/bin:/bin" FAKE_FIXTURES="$fixtures" FAKE_OS=Darwin FAKE_ARCH=arm64 \
        sh "$INSTALL" --version "$VERSION" --dir "$tmp/out" 2>&1); then
    if [ -x "$tmp/out/ki" ]; then
        echo "ok: installs on a stock macOS PATH, where sha256sum is absent"
    else
        echo "FAIL: install reported success but $tmp/out/ki is missing: $out" >&2
        status=1
    fi
else
    echo "FAIL: install failed on a stock macOS PATH: $out" >&2
    status=1
fi

# A tampered archive must be rejected, not installed.
cp "$tmp/pristine.tar.gz" "$fixtures/$asset"
printf 'corrupted' >> "$fixtures/$asset"
rm -rf "$tmp/out2"
if out=$(PATH="$tmp/bin:/usr/bin:/bin" FAKE_FIXTURES="$fixtures" FAKE_OS=Darwin FAKE_ARCH=arm64 \
        sh "$INSTALL" --version "$VERSION" --dir "$tmp/out2" 2>&1); then
    echo "FAIL: a corrupted archive was installed without complaint" >&2
    status=1
else
    case "$out" in
        *"checksum verification failed"*) echo "ok: corrupted archive rejected by checksum" ;;
        *) echo "FAIL: corrupted archive rejected, but not for the checksum: $out" >&2; status=1 ;;
    esac
fi
cp "$tmp/pristine.tar.gz" "$fixtures/$asset"

# A missing checksum must fail, matching install.ps1. Tolerating it was the original
# behaviour and it disagreed with the Windows installer; since the workflow publishes one
# for every archive (v0.4.0 and v0.4.1: 5 of 5), tolerating it only masks a broken publish.
nosec="$tmp/nosec"
mkdir -p "$nosec"
cp "$tmp/pristine.tar.gz" "$nosec/$asset"
cat > "$nosec/curl" <<'SHIM'
#!/bin/sh
# Serve the archive but 404 on any .sha256, i.e. a release missing its checksums.
dest=""; url=""
while [ $# -gt 0 ]; do
    case "$1" in
        -o) dest=$2; shift 2 ;;
        -*) shift ;;
        *) url=$1; shift ;;
    esac
done
case "$url" in *.sha256) exit 22 ;; esac
cp "$FAKE_FIXTURES/$(basename "$url")" "$dest"
SHIM
chmod +x "$nosec/curl"
rm -rf "$tmp/out6"
if out=$(PATH="$nosec:$tmp/bin:/usr/bin:/bin" FAKE_FIXTURES="$fixtures" FAKE_OS=Darwin FAKE_ARCH=arm64 \
        sh "$INSTALL" --version "$VERSION" --dir "$tmp/out6" 2>&1); then
    echo "FAIL: installed without a published checksum" >&2
    status=1
else
    case "$out" in
        *"refusing to install unverified bytes"*) echo "ok: a missing checksum fails the install" ;;
        *) echo "FAIL: failed for another reason: $out" >&2; status=1 ;;
    esac
fi

# A published checksum with no way to verify it must fail loudly, not install.
if command -v env >/dev/null 2>&1; then
    nover="$tmp/nover"
    mkdir -p "$nover"
    # `command -v` skips non-executable files, so these shadow nothing but are also not
    # found anywhere else: /usr/bin is excluded from PATH below.
    for tool in sha256sum shasum openssl; do : > "$nover/$tool"; done
    # `uname` must come from the shim, not the host: the fixture archive is named for
    # Darwin/arm64, so a host `uname` on Linux would resolve a different target and 404.
    for tool in sh dirname sed head mkdir rm cp tar chmod cut env mktemp cat printf ls mv; do
        real=$(command -v "$tool") || continue
        ln -sf "$real" "$nover/$tool" 2>/dev/null || true
    done
    ln -sf "$tmp/bin/uname" "$nover/uname"
    ln -sf "$tmp/bin/curl" "$nover/curl"
    rm -rf "$tmp/out3"
    if out=$(PATH="$nover" FAKE_FIXTURES="$fixtures" FAKE_OS=Darwin FAKE_ARCH=arm64 \
            sh "$INSTALL" --version "$VERSION" --dir "$tmp/out3" 2>&1); then
        echo "FAIL: checksum published but unverifiable, yet install succeeded" >&2
        status=1
    else
        case "$out" in
            *"no sha256 tool is available"*) echo "ok: refuses to install unverified bytes" ;;
            *) echo "FAIL: refused, but not for the documented reason: $out" >&2; status=1 ;;
        esac
    fi

    # Third fallback: `openssl dgst -sha256`. Reached only when neither sha256sum nor
    # shasum exists, which the `$nover` PATH cannot produce (it excludes both). Build a
    # PATH that has openssl and lacks the other two, and prove it both accepts a good
    # archive and rejects a tampered one -- otherwise the branch is dead code pretending
    # to be a safety net.
    ossl="$tmp/ossl"
    mkdir -p "$ossl"
    for tool in sha256sum shasum; do : > "$ossl/$tool"; done
    for tool in sh dirname sed head mkdir rm cp tar chmod cut env mktemp cat printf ls mv; do
        real=$(command -v "$tool") || continue
        ln -sf "$real" "$ossl/$tool" 2>/dev/null || true
    done
    ln -sf "$tmp/bin/uname" "$ossl/uname"
    ln -sf "$tmp/bin/curl" "$ossl/curl"
    real_openssl=$(command -v openssl || true)
    if [ -n "$real_openssl" ]; then
        ln -sf "$real_openssl" "$ossl/openssl"
        rm -rf "$tmp/out4"
        if PATH="$ossl" FAKE_FIXTURES="$fixtures" FAKE_OS=Darwin FAKE_ARCH=arm64 \
                sh "$INSTALL" --version "$VERSION" --dir "$tmp/out4" >/dev/null 2>&1 \
                && [ -x "$tmp/out4/ki" ]; then
            echo "ok: openssl fallback verifies and installs"
        else
            echo "FAIL: the openssl fallback could not install a good archive" >&2
            status=1
        fi

        cp "$tmp/pristine.tar.gz" "$fixtures/$asset"
        printf 'corrupted' >> "$fixtures/$asset"
        rm -rf "$tmp/out5"
        if out=$(PATH="$ossl" FAKE_FIXTURES="$fixtures" FAKE_OS=Darwin FAKE_ARCH=arm64 \
                sh "$INSTALL" --version "$VERSION" --dir "$tmp/out5" 2>&1); then
            echo "FAIL: openssl fallback installed a corrupted archive" >&2
            status=1
        else
            case "$out" in
                *"checksum verification failed"*) echo "ok: openssl fallback rejects a tampered archive" ;;
                *) echo "FAIL: openssl fallback failed for another reason: $out" >&2; status=1 ;;
            esac
        fi
        cp "$tmp/pristine.tar.gz" "$fixtures/$asset"
    else
        echo "note: openssl not on PATH here; fallback branch not exercised" >&2
    fi
fi

exit "$status"
