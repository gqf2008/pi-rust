# Contributing to ki

Thanks for taking the time to contribute. This document covers the practical
details: how to build, what CI requires, and how to get a change merged.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).

## Ways to contribute

- **Bug reports** — use the [bug report form](../../issues/new?template=bug_report.yml).
  A minimal reproduction (command, config, session file) is worth more than a
  long description.
- **Feature requests** — use the [feature request form](../../issues/new?template=feature_request.yml).
  Describe the problem first, then the proposed solution.
- **Extensions** — wasm guests live in
  [`extensions/wasm/`](extensions/wasm) and are built with `ki insmod --build`.
  Bugs in the ABI or the loader itself belong in this repository too.
- **Documentation** — `README.md`, `docs/`, and the static site in `website/`
  are all fair game. Typos and broken links are welcome as direct PRs.
- **Code** — see below.

## Prerequisites

- Rust **1.96 or newer**. `rust-toolchain` is not pinned, so please test with
  stable; for MSRV-sensitive changes, test with the version declared as
  `rust-version` in `Cargo.toml` (`rustup run <version> cargo check --workspace`
  — a two-part version like `1.96` means "the newest 1.96.x"). CI's `msrv` job
  enforces the same thing, reading the declared value rather than a copy.
- A C toolchain for `rusqlite` (`bundled`) and, on Windows, the MSVC linker.
- Optional: [`task`](https://taskfile.dev/) for the release/deploy targets in
  `Taskfile.yml`. Ordinary builds do not need it.

No API key is required for the test suite: `kiqi-ai` ships a deterministic
`faux` provider and the tests run offline.

## Build and test

```bash
cargo build --workspace --locked
cargo test  --workspace --locked
```

Before opening a PR, run what CI enforces:

```bash
cargo check --workspace --all-targets --locked
cargo test  --workspace --locked
```

Formatting: run `cargo fmt --all -- --check` before opening a PR. CI runs the
same check. The repo-wide formatting sweep is listed in
`.git-blame-ignore-revs`; to hide it in local blame, run:

```bash
git config blame.ignoreRevsFile .git-blame-ignore-revs
```

Clippy is not part of CI either, so keep new code warning-free by hand:

```bash
cargo clippy --workspace --all-targets --locked
```

Network-dependent tests are gated. Do not add a test that requires a live
provider to the default `cargo test` path.

## Repository layout

The workspace directories and the published crate names both use the `kiqi-`
prefix (`crates/kiqi-ai` ↔ `kiqi-ai`). `package.name` in each `Cargo.toml` is the
crate name; the directory name is not part of the public API.

| Directory                          | Crate            |
| ---------------------------------- | ---------------- |
| `crates/kiqi-telemetry`              | `kiqi-telemetry`  |
| `crates/kiqi-ai`                     | `kiqi-ai`         |
| `crates/kiqi-agent`                  | `kiqi-agent`      |
| `crates/kiqi-tools`                  | `kiqi-tools`      |
| `crates/kiqi-harness`                | `kiqi-harness`    |
| `crates/kiqi-cli`                    | `kiqi-cli`        |
| `crates/kiqi-tui`                    | `kiqi-tui`        |
| `crates/kiqi-host`                   | `kiqi-host`       |

Dependency direction is one-way and must stay that way:

```
kiqi-telemetry → kiqi-ai → kiqi-agent → kiqi-tools → kiqi-harness → kiqi-cli
```

`kiqi-host` sits beside the runtime chain: its contract and dispatch spine
build without wasmtime (so `kiqi-tools` can read the env-var names), and the
wasmtime backend is behind the `wasm` feature that only `kiqi-cli` enables.
Adding a dependency that points backwards (for example `kiqi-agent` depending
on `kiqi-cli`) will be rejected.

`docs/architecture/overview.md` explains the design intent behind the split;
`docs/agent/project-structure.md` is the recommended structure for projects built on top
of the crates.

## Coding guidelines

- Keep the public API of a crate additive within a minor version. Breaking
  changes to a published crate need a version bump and a `CHANGELOG.md` entry.
- Prefer the smallest change that solves the problem. Refactors that are not
  required by the fix belong in a separate PR.
- New behaviour needs a test. Prefer the in-memory execution environment and
  the faux provider over anything that touches the network or the user's real
  filesystem.
- Error messages should name the failing component and what the user can do
  next.
- Run `cargo fmt`. There is no separate style document beyond rustfmt defaults.
- Public items should carry a doc comment. `#![warn(missing_docs)]` is not
  enabled workspace-wide yet, so this is a review expectation rather than a
  build failure.

## Commit messages

The history follows [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(cli): add --timeout override
fix(tools): preserve @ when accepting file autocomplete
docs: sync website package data
chore: release v0.3.0
```

Keep the subject in the imperative mood and under ~72 characters. Chinese and
English subjects are both present in the history; either is acceptable, but
keep the type prefix and scope in the conventional form.

## Pull requests

1. Fork, branch from `main`, and keep the branch focused on one change.
2. Fill in the PR template — including which crates are affected and how you
   verified the change.
3. Make sure CI is green. `Cargo test` and `Cargo check` both have to pass.
4. Expect review comments on API additions; the public surface of these crates
   is intended to be small and stable.

If your change touches the extension contract (`extensions/kiqi-extension.wit`,
`kiqi-host`), call it out explicitly in the PR description. Contract changes
affect every already-built wasm extension and require a documented negotiation
story (`docs/design/extension-abi.md`).

If your change touches session persistence (`kiqi-harness`), describe the
recovery behaviour for a crash in the middle of the affected operation.

## Releases

Releases are cut by a maintainer, not by contributors:

1. `CHANGELOG.md` is updated and the version is bumped in the workspace
   `Cargo.toml`.
2. `task publish` derives the next version, bumps/tags/pushes it and runs the
   whole channel + crates.io pipeline; `task publish:crates` publishes the nine
   crates in dependency order without touching git.
3. `git tag vX.Y.Z` is pushed and a matching GitHub Release is created; CI
   publishes automatically on pushes to `main` once the version bump lands.

Do not bump a version or publish a crate in a PR.

## Reporting security issues

Do not open a public issue. See [SECURITY.md](SECURITY.md).
