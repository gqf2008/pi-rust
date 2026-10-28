# Install channels

ki has one live binary install path today (`scripts/install.sh`), plus a source build.
The package-manager manifests in this directory are prepared for the project's
own owner (`gqf2008`) but have **not been published**; the taps under
`bigfish1913` belong to an unrelated fork and carry a different tool (`rpi`).
Every manifest still points at a **real release asset** with its **real
checksum** — none of it is a template with placeholders.

## Publication decision (2026-10-20)

**No publication in this cycle.** The supported binary paths are
`scripts/install.sh`, a source build, and `cargo binstall` once the crates are
actually published (they remain `publish = false`). No crates, Homebrew/Scoop
taps, or winget package are published; the catalogue, README and
`docs-install-path.sh` state that, and the site no longer promises
`cargo install`/`ki install` for unpublished packages.

The manifests and scripts already point at `gqf2008/pi-rust` v0.4.4 with real
digests. Publishing is a later owner decision: create the `gqf2008` taps, run
`scripts/push-taps.sh`, submit a `gqf2008.ki` winget manifest, and publish the
crates; only then should the catalogue claims change.

## Status

| Channel | Lives in | Status | Remaining step |
| --- | --- | --- | --- |
| `cargo install kiqi-cli` | crates.io | **not published** | every crate is `publish = false`; use `install.sh` or a source build |
| `curl … install.sh \| sh` | [`scripts/install.sh`](../scripts/install.sh) | **live** | none |
| `cargo binstall kiqi-cli` | [`crates/kiqi-cli/Cargo.toml`](../crates/kiqi-cli/Cargo.toml) | **not available** | takes effect only after the crates are published (`publish = false` today) |
| Homebrew | `gqf2008/homebrew-tap` | **not published** | create the tap, then `scripts/push-taps.sh`; `bigfish1913/homebrew-tap` serves `rpi`, not `ki` |
| Scoop | `gqf2008/scoop-bucket` | **not published** | create the bucket, then `scripts/push-taps.sh`; `bigfish1913/scoop-bucket` serves `rpi`, not `ki` |
| winget | [open at `microsoft/winget-pkgs#441442`](https://github.com/microsoft/winget-pkgs/pull/441442) | **not published** | the open PRs are for the unrelated fork's `bigfish1913.rpi` package; no `gqf2008.ki` submission exists |

The files in this directory are the source of truth for the two taps; the tap
repos are copies. `scripts/push-taps.sh` refuses to run unless the target
`gqf2008/*` repos exist, so it can never fall back to the unrelated fork.
Homebrew-in-`homebrew-core` and `winget` carry the real discovery weight — both
index by name inside a package manager the user already has open, which is
traffic the website cannot reach.

## Cutting a release

The whole pipeline — bump, tag, wait for the release binaries, refresh every
manifest below, push the taps, open the winget PR, (skip crates.io while
`publish = false`), deploy
the site — is one command; see [`docs/maintaining/releasing.md`](../docs/maintaining/releasing.md):

```bash
task publish                 # auto-increments the version
task release RELEASE_VERSION=0.3.4   # explicit version
```

Add `-- --dry-run` to print the plan without executing anything.

## The asset contract these manifests depend on

`.github/workflows/release-binaries.yml` publishes one archive per target, **with
the binary at the archive root** and a `.sha256` beside every archive.

The table below records the original four targets and the digests that v0.3.2
actually published — asset names changed after it (`kiqi-v…` → `ki-v…`, and the
version now comes without its `v` prefix; see `refresh-channels.sh`). The workflow
also builds Intel macOS as `ki-v<version>-x86_64-apple-darwin.tar.gz`, with a
matching `.sha256`; this asset is available in releases whose tag includes that
target.

| Target | Asset | sha256 (v0.3.2) |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `kiqi-v0.3.2-x86_64-unknown-linux-gnu.tar.gz` | `d5ec1b404ffad0e0d0e3a32f43ae70334922a71e1a156a16ce66ed092c2c3a1a` |
| `aarch64-unknown-linux-gnu` | `kiqi-v0.3.2-aarch64-unknown-linux-gnu.tar.gz` | `5dd459b6968c64438f2f4b16cb8142f939bfad8ae72302e4729feb6d2c5a6ac7` |
| `aarch64-apple-darwin` | `kiqi-v0.3.2-aarch64-apple-darwin.tar.gz` | `1a99c43c6f158e3cb437724a501cbf654edb79f14f1b05df6d1991aee9e1db00` |
| `x86_64-pc-windows-msvc` | `kiqi-v0.3.2-x86_64-pc-windows-msvc.zip` | `2015c81921867a106ac79fc2721af6d28eb4010f7148c77dbd4283065c659307` |

Two consequences worth knowing:

- Intel Macs can use `scripts/install.sh` for releases containing that asset,
  or `cargo install --path crates/kiqi-cli --force` from a checkout. The
  Homebrew formula currently covers Apple silicon on macOS.
- The assets are named after the **binary** (`ki`), while the crate is
  `kiqi-cli`. Anything templating a URL from the crate name is wrong — which is
  why the binstall `pkg-url` uses `{ bin }` and not `{ name }`.

## Refresh procedure for a new release

`curl` against `github.com/.../releases/download/...` returns nothing from some
networks (this machine included), so use `gh`, which goes through the API:

```bash
TAG=v0.4.4                      # the tag being packaged
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
# 4. PUT each file to manifests/b/gqf2008/ki/$V/ via the contents API.
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
  `gqf2008.ki` / `0.4.1` manifests were edited in this batch and have not been
  re-validated here (no `winget` on this machine).
- The current v0.4.4 URLs and hashes were read from the GitHub API
  (`releases/tags/v0.4.4`, `assets[].digest`), not copied from a template. The
  `v0.3.2` table below is historical and is not what the manifests carry now.
- `scoop/ki.json` parses, and its `bin` (`ki.exe`) matches the zip's root
  entry produced by the workflow's `7z a -tzip … .` step.
- The v0.4.4 digests recorded in `scoop/ki.json` and `homebrew/ki.rb` match
  the GitHub API's `assets[].digest` (Windows zip `1edc47c0…e273`, macOS arm64
  tarball `f7f894fa…ceec`), and the API's asset list confirms the filenames the
  manifests build. A downloaded archive was not re-hashed in this pass; older
  releases had `ki.exe` at the zip root, which is what `bin` claims.
- The `gqf2008/*` taps do **not** exist yet, so `brew install ki` and
  `scoop install ki` remain unavailable; `scripts/push-taps.sh` preflights both
  repos and refuses to push elsewhere.

**Not** verified here, and worth one manual pass before relying on them:

- **Homebrew** — no `brew`/`ruby` on this machine, so the formula was never
  syntax-checked or installed (nothing is published yet). `brew audit
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
