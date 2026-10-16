# Install channels

ki reaches users six ways. Five are live today and the sixth is a manifest
sitting in review. The point of this directory is that every manifest points at
a **real release asset** and carries that asset's **real checksum** — none of it
is a template with placeholders.

## Status

| Channel | Lives in | Status | Remaining step |
| --- | --- | --- | --- |
| `cargo install kiqi-cli` | crates.io | **live** | none |
| `curl … install.sh \| sh` | [`scripts/install.sh`](../scripts/install.sh) | **live** | none |
| `cargo binstall kiqi-cli` | [`crates/kiqi-cli/Cargo.toml`](../crates/kiqi-cli/Cargo.toml) | **configured** | none — takes effect with the next published crate version |
| Homebrew | [`bigfish1913/homebrew-tap`](https://github.com/bigfish1913/homebrew-tap) | **live** | none — `brew tap bigfish1913/tap && brew install ki` |
| Scoop | [`bigfish1913/scoop-bucket`](https://github.com/bigfish1913/scoop-bucket) | **live** | none — `scoop bucket add bigfish1913 <bucket url>` |
| winget | [open at `microsoft/winget-pkgs#441442`](https://github.com/microsoft/winget-pkgs/pull/441442) | **submitted** | review by the winget team; `winget install bigfish1913.ki` does not work until it merges |

The files in this directory are the source of truth for the two taps; the tap
repos are copies. Homebrew-in-`homebrew-core` and `winget` carry the real
discovery weight — both index by name inside a package manager the user already
has open, which is traffic the website cannot reach.

## Cutting a release

The whole pipeline — bump, tag, wait for the release binaries, refresh every
manifest below, push the taps, open the winget PR, publish to crates.io, deploy
the site — is one command; see [`docs/maintaining/releasing.md`](../docs/maintaining/releasing.md):

```bash
task publish                 # auto-increments the version
task release RELEASE_VERSION=0.3.4   # explicit version
```

Add `-- --dry-run` to print the plan without executing anything.

## The asset contract these manifests depend on

`.github/workflows/release-binaries.yml` publishes one archive per target, **with
the binary at the archive root** and a `.sha256` beside every archive.

The table below records the original four targets. The workflow also builds
Intel macOS as `kiqi-v<version>-x86_64-apple-darwin.tar.gz`, with a matching
`.sha256`; this asset is available in releases whose tag includes that target.

| Target | Asset | sha256 (v0.3.2) |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `kiqi-v0.3.2-x86_64-unknown-linux-gnu.tar.gz` | `9ceeb5fe434fbeca9c27ef9f4b622ebb9652cc5563d488664529a8d501367afd` |
| `aarch64-unknown-linux-gnu` | `kiqi-v0.3.2-aarch64-unknown-linux-gnu.tar.gz` | `6ad98e3908a3fd952034d49d98b973300914b82e4867df30dcd08041940516a5` |
| `aarch64-apple-darwin` | `kiqi-v0.3.2-aarch64-apple-darwin.tar.gz` | `8a6a96bbf99265acb7a34ad560ed3de3a76916755bfcbb7264a9a3bae4910690` |
| `x86_64-pc-windows-msvc` | `kiqi-v0.3.2-x86_64-pc-windows-msvc.zip` | `f2b2511aaf8c0b2dfe127e3e04ac796628d985b0b915cfce9a73a8e472aa8485` |

Two consequences worth knowing:

- Intel Macs can use `scripts/install.sh` for releases containing that asset,
  or `cargo install kiqi-cli`. The Homebrew formula currently covers Apple
  silicon on macOS.
- The assets are named after the **binary** (`ki`), while the crate is
  `kiqi-cli`. Anything templating a URL from the crate name is wrong — which is
  why the binstall `pkg-url` uses `{ bin }` and not `{ name }`.

## Refresh procedure for a new release

`curl` against `github.com/.../releases/download/...` returns nothing from some
networks (this machine included), so use `gh`, which goes through the API:

```bash
TAG=v0.3.21                      # the tag being packaged
gh release download "$TAG" --pattern "*.sha256" --dir /tmp/kiqi-sha --clobber
cat /tmp/kiqi-sha/*.sha256
```

Then update, in this order:

1. **binstall** — nothing to change; the URL is templated from `{ version }`.
2. **Scoop** — `scoop/ki.json`: `version`, the `architecture.64bit.url`, and
   `architecture.64bit.hash`. `checkver`/`autoupdate` keep later releases
   automatic.
3. **Homebrew** — `homebrew/ki.rb`: `version` and the three `url`/`sha256`
   pairs.
4. **winget** — all three files' `PackageVersion`, the `InstallerUrl`, and the
   **uppercase** `InstallerSha256`.

The winget manifests track the **current** repo convention rather than the
oldest supported schema: `ManifestVersion 1.12.0` with the
`yaml-language-server` schema comment, and `NestedInstallerFiles` inside each
installer entry. That is what the newest manifests merged into
`microsoft/winget-pkgs` look like (checked against `BurntSushi.ripgrep.MSVC
15.2.0`), so a reviewer does not have to ask for the newer shape.

### Submitting to winget

Done for 0.3.3 — [PR #441442](https://github.com/microsoft/winget-pkgs/pull/441442)
(the 0.3.2 submission, #441408, was closed as superseded rather than left open:
two open PRs for one package is duplicate work for a moderator, and the CLA
signature carries over). For the next version, repeat it without cloning the
866 MB monorepo; every step below is a server-side API call:

```bash
V=0.3.4
winget validate packaging/winget                       # 1. validate what you will submit
gh repo fork microsoft/winget-pkgs --clone=false       # 2. server-side fork, no download
UP=$(gh api repos/microsoft/winget-pkgs/commits/master --jq .sha)
gh api --method POST repos/<you>/winget-pkgs/git/refs \
  -f ref="refs/heads/kiqi-$V" -f sha="$UP"               # 3. branch off upstream master
# 4. PUT each file to manifests/b/bigfish1913/ki/$V/ via the contents API.
#    Use `--input` with a JSON body: the README-sized payloads blow past the
#    ~32 KB command-line limit if you pass them with -f content=.
gh pr create --repo microsoft/winget-pkgs --base master --head <you>:kiqi-$V \
  --title "New package: bigfish1913.ki version $V"     # 5. open the PR
```

One trap in the CLA step, worth not rediscovering: the policy bot matches
`@microsoft-github-policy-service agree` **anchored to the start of the
comment**, so a single leading space fails the match. The bot does not report
that — it silently re-posts the CLA text, which reads exactly like the signature
never arrived. The comment body must begin with `@` at column 0 and contain
nothing else on the line.

Also pick the right form: the bare `agree` asserts you are **not** contributing
as part of work for an employer. If you are, use `agree company="…"`.

## What was verified, and what was not

Verified on this machine:

- `winget validate packaging/winget` → **Manifest validation succeeded.**
- Every URL and hash above was read back from the published v0.3.2 assets
  (`gh release download --pattern '*.sha256'`), not copied from a template.
- `scoop/ki.json` parses, and its `bin` (`ki.exe`) matches the zip's root
  entry produced by the workflow's `7z a -tzip … .` step.
- The published archives themselves were downloaded and hashed here, byte for
  byte: the Windows zip is `c5b572d7…0d7f` and the macOS tarball is
  `92e1851f…1842`, matching `scoop/ki.json` and `homebrew/ki.rb` exactly. The
  zip was listed to confirm `ki.exe` really is at the archive root — the thing
  `bin` claims and the only reason the Scoop manifest works.
- Both taps are pushed and public (`bigfish1913/homebrew-tap`,
  `bigfish1913/scoop-bucket`), so their files are fetchable at the URLs the
  package managers will use.

**Not** verified here, and worth one manual pass before relying on them:

- **Homebrew** — no `brew`/`ruby` on this machine, so the formula was never
  syntax-checked or installed, even though it is published. `brew audit
  --new-formula ki` in the tap is the cheap check.
- **binstall** — `cargo-binstall` is not installed here, so the metadata is
  written from the documented schema (`SUPPORT.md` in `cargo-bins/cargo-binstall`)
  but untested. `cargo binstall --no-confirm kiqi-cli` against the next published
  version is the test. Note the docs do **not** mention verifying our `.sha256`
  files, so do not claim checksum verification for this channel.
- **Scoop** — the manifest was not installed from; `scoop checkver` in the real
  bucket is the check.
- A `winget install` was deliberately **not** run, because it would modify this
  machine's package state — and the package is not installable yet anyway, since
  the manifest is an open PR.
