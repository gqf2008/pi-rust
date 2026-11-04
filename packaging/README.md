# Install channels

ki ships prebuilt binaries through `scripts/install.sh` and the published
Homebrew/Scoop taps, plus a source build.
The package-manager manifests in this directory are for the project's own owner
(`gqf2008`); the taps under `bigfish1913` belong to an unrelated project and
carry a different tool (`rpi`).
Every manifest still points at a **real release asset** with its **real
checksum** — none of it is a template with placeholders.

## Publication decision (2026-10-20)

**No publication in this cycle.** The supported binary paths are
`scripts/install.sh`, a source build, and `cargo binstall` once the crates are
actually published (they remain `publish = false`). No crates, Homebrew/Scoop
taps, or winget package are published; the catalogue, README and
`docs-install-path.sh` state that, and the site no longer promises
`cargo install`/`ki install` for unpublished packages.

**Superseded:** the `gqf2008` taps were created on 2026-10-27 and now carry the
current release; winget submissions stay open PRs until a moderator merges them
(see Status).

The manifests and scripts already point at `gqf2008/pi-rust` releases with real
digests. Publishing is a later owner decision: create the
`gqf2008` taps, run `scripts/push-taps.sh`, submit a `gqf2008.ki` winget
manifest, and publish the crates; only then should the catalogue claims change.

## Status

| Channel | Lives in | Status | Remaining step |
| --- | --- | --- | --- |
| `cargo install kiqi-cli` | crates.io | **not published** | every crate is `publish = false`; use `install.sh` or a source build |
| `curl … install.sh \| sh` | [`scripts/install.sh`](../scripts/install.sh) | **live** | none |
| `cargo binstall kiqi-cli` | [`crates/kiqi-cli/Cargo.toml`](../crates/kiqi-cli/Cargo.toml) | **not available** | takes effect only after the crates are published (`publish = false` today) |
| Homebrew | `gqf2008/homebrew-tap` | **published (current release)** | `brew tap gqf2008/tap && brew install ki`; `bigfish1913/homebrew-tap` serves `rpi`, not `ki` |
| Scoop | `gqf2008/scoop-bucket` | **published (current release)** | `scoop bucket add gqf2008 https://github.com/gqf2008/scoop-bucket && scoop install ki`; `bigfish1913/scoop-bucket` serves `rpi`, not `ki` |
| winget | [open at `microsoft/winget-pkgs#469391`](https://github.com/microsoft/winget-pkgs/pull/469391) | **submitted, pending moderator merge** | PR #469391 adds `gqf2008.ki` 0.5.3 under `manifests/g/…`; #441442 is the unrelated `bigfish1913.rpi` package |

The files in this directory are the source of truth for the two taps; the tap
repos are copies. `scripts/push-taps.sh` refuses to run unless the target
`gqf2008/*` repos exist, so it can never fall back to the unrelated fork.
Homebrew-in-`homebrew-core` and `winget` carry the real discovery weight — both
index by name inside a package manager the user already has open, which is
traffic the website cannot reach.

## Cutting a release

The whole pipeline — bump, tag, wait for the release binaries, refresh every
manifest below, push the taps, open the winget PR (skipping crates.io while
`publish = false`) — is one command; see
[`docs/maintaining/releasing.md`](../docs/maintaining/releasing.md):

```bash
task publish                 # auto-increments the version
task release RELEASE_VERSION=0.3.4   # explicit version
```

Add `-- --dry-run` to print the plan without executing anything.

## The asset contract these manifests depend on

`.github/workflows/release-binaries.yml` publishes one archive per target, **with
the binary at the archive root** and a `.sha256` beside every archive.

The table below records the four targets and the digests the current release
actually published — asset names changed after v0.3.2 (`kiqi-v…` → `ki-v…`, and
the version now comes without its `v` prefix; see `refresh-channels.sh`). The workflow
also builds Intel macOS as `ki-v<version>-x86_64-apple-darwin.tar.gz`, with a
matching `.sha256`; this asset is available in releases whose tag includes that
target.

| Target | Asset | sha256 (v0.5.6) |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ki-v0.5.6-x86_64-unknown-linux-gnu.tar.gz` | `0477c348c1c73f97a05e6c4df58021c953e1101367b0d7d9cbbdcb8013e67eff` |
| `aarch64-unknown-linux-gnu` | `ki-v0.5.6-aarch64-unknown-linux-gnu.tar.gz` | `f885e462fe1a7b77b1ac7ba427d7970a3546e0045a62da8a4343ab0998895347` |
| `aarch64-apple-darwin` | `ki-v0.5.6-aarch64-apple-darwin.tar.gz` | `f69bebeabdd3d81f73e2cc0bbf29024a334801604b5897ed759256f2d3a57241` |
| `x86_64-pc-windows-msvc` | `ki-v0.5.6-x86_64-pc-windows-msvc.zip` | `59a2fca659b5248fc2bc5217fa0afbe4241eb27b6a7f1fd89b82628f0979f972` |

Two consequences worth knowing:

- Intel Macs can use `scripts/install.sh` for releases containing that asset,
  or `cargo install --path crates/kiqi-cli --force` from a checkout. The
  Homebrew formula currently covers Apple silicon on macOS.
- The assets are named after the **binary** (`ki`), while the crate is
  `kiqi-cli`. Anything templating a URL from the crate name is wrong — which is
  why the binstall `pkg-url` uses `{ bin }` and not `{ name }`.

## Refresh procedure for a new release

`curl` against `github.com/.../releases/download/...` returns nothing from some
networks (this machine included), and `gh release download` follows the same
release-asset host. Read the digests from the API instead, which is what
`scripts/refresh-channels.sh` does:

```bash
TAG=v0.5.6                      # the tag being packaged
gh api "repos/gqf2008/pi-rust/releases/tags/$TAG" \
  --jq '.assets[] | .name + " " + (.digest // "NO_DIGEST")'
```

Then update, in this order:

1. **binstall** — nothing to change; the URL is templated from `{ version }`.
2. **Scoop** — `scoop/ki.json`: `version`, the `architecture.64bit.url`, and
   `architecture.64bit.hash`. `checkver`/`autoupdate` keep later releases
   automatic.
3. **Homebrew** — `homebrew/ki.rb`: `version` and the three `url`/`sha256`
   pairs.
4. **winget** — all three files' `PackageVersion`, the `InstallerUrl`, the
   **uppercase** `InstallerSha256`, the locale manifest's `ReleaseNotesUrl`, and
   the installer's `ReleaseDate`.

The winget manifests track the **current** repo convention rather than the
oldest supported schema: `ManifestVersion 1.12.0` with the
`yaml-language-server` schema comment, and `NestedInstallerFiles` inside each
installer entry. That is what the newest manifests merged into
`microsoft/winget-pkgs` look like (checked against `BurntSushi.ripgrep.MSVC
15.2.0`), so a reviewer does not have to ask for the newer shape.

### Submitting to winget

Submitted for 0.5.3 — [PR #469391](https://github.com/microsoft/winget-pkgs/pull/469391)
(an earlier 0.4.7 submission, #467025, was closed). For the next version, repeat
it without cloning the 866 MB monorepo; every step below is a server-side API
call:

```bash
V=0.3.4
winget validate packaging/winget                       # 1. validate what you will submit
gh repo fork microsoft/winget-pkgs --clone=false       # 2. server-side fork, no download
UP=$(gh api repos/microsoft/winget-pkgs/commits/master --jq .sha)
gh api --method POST repos/<you>/winget-pkgs/git/refs \
  -f ref="refs/heads/kiqi-$V" -f sha="$UP"               # 3. branch off upstream master
# 4. PUT each file to manifests/g/gqf2008/ki/$V/ via the contents API.
#    Use `--input` with a JSON body: the README-sized payloads blow past the
#    ~32 KB command-line limit if you pass them with -f content=.
gh pr create --repo microsoft/winget-pkgs --base master --head <you>:kiqi-$V \
  --title "New package: gqf2008.ki version $V"     # 5. open the PR
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

- `winget validate packaging/winget` validated the old `0.3.21` manifests. The
  current `gqf2008.ki` manifests have not been re-validated here (no `winget`
  on this machine).
- The v0.5.6 URLs and hashes were read from the GitHub API
  (`releases/tags/v0.5.6`, `assets[].digest`), not copied from a template.
- `scoop/ki.json` parses, and its `bin` (`ki.exe`) matches the zip's root
  entry produced by the workflow's `7z a -tzip … .` step.
- The v0.5.6 digests recorded in `scoop/ki.json` and `homebrew/ki.rb` match
  the GitHub API's `assets[].digest` (Windows zip `59a2fca6…f972`, macOS arm64
  tarball `f69bebea…7241`), and the API's asset list confirms the filenames the
  manifests build. A downloaded archive was not re-hashed in this pass; older
  releases had `ki.exe` at the zip root, which is what `bin` claims.
- The `gqf2008/*` taps exist and carry the current release; `brew install ki` /
  `scoop install ki` were not exercised here. `scripts/push-taps.sh` preflights
  both repos and refuses to push elsewhere.

**Not** verified here, and worth one manual pass before relying on them:

- **Homebrew** — no `brew`/`ruby` on this machine, so the formula was never
  syntax-checked or installed. `brew audit
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
