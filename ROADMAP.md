# Roadmap

This is a direction document, not a commitment. It records what the project is
working on next and why, so that issues and PRs can be judged against it. For
what has already shipped, see [CHANGELOG.md](CHANGELOG.md).

Current release: **0.4.7**. Install with `scripts/install.sh` / `install.ps1`, or
build from source — the crates are not on crates.io, and the package-manager
taps are staged under `packaging/` but not published yet (`packaging/README.md`).

## Where the project is now

`ki` is a Rust-native coding-agent runtime: a terminal agent on top of ten
`kiqi-*` crates — the core chain (`kiqi-telemetry` → `kiqi-ai` → `kiqi-agent` →
`kiqi-tools` → `kiqi-harness` → `kiqi-cli`), plus `kiqi-tui`, `kiqi-host`,
`kiqi-mcp` and `kiqi-acp`. Extensions are wasm guests (the cdylib channel was
retired on 2026-10-26).

What is solid today:

- Async, streaming agent loop with tool calls, hooks, queues, cancellation and
  a faux provider for offline tests.
- Anthropic Messages and OpenAI-compatible providers, plus custom gateways via
  `models.json`.
- Durable JSONL sessions with branching, compaction and crash recovery.
- A sandboxed wasm extension channel (`extensions/kiqi-extension.wit`) with grants,
  lifecycle events and veto support.
- Remote mode: a headless `--server` and a zero-local-resource `--connect`
  client.
- Resource layout: `.ki/` project resources and `settings.json` keys.

## Near term

### 1. Distribution without a Rust toolchain

Shipped in 0.4.7: prebuilt `x86_64`/`aarch64` binaries for Linux, macOS and
Windows, and `scripts/install.sh` / `install.ps1`, which verify the published
SHA-256 before installing. Nothing here needs a Rust toolchain any more. The
Homebrew formula, the Scoop manifest and the winget package are written and
refreshed from one release by `scripts/refresh-channels.sh`, but they are **not
published** yet — `packaging/README.md` tracks each channel.

### 2. Documentation that stands on its own

- Per-crate README and `lib.rs` landing docs, so each crate is usable without
  the workspace README (there are no crates.io/docs.rs pages — `publish = false`).
- A task-oriented tutorial that goes from `scripts/install.sh` to a working
  custom tool, in one page.
- Keep the parity story honest and measured: `docs/performance/benchmark-vs-pi.md`
  holds the numbers, and the README says plainly what `ki` does not do. A
  compatibility claim without either is worse than none.

### 3. Provider breadth

The provider boundary is in place; the gaps are adapters. Shipped today:
Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, plus
OpenAI-compatible gateways including OpenRouter, DeepSeek and llama.cpp.
Candidates next, in rough order of demand: Google Gemini, AWS Bedrock, Azure
OpenAI, and a first-class Ollama preset.

### 4. Extension ecosystem

The ABI and loader work; the ecosystem is thin. Concretely:

- Keep the reference guests in [`extensions/wasm/`](extensions/wasm) building
  green against the current ABI and publish a compatibility table per ABI
  version.
- Ship an extension template (`cargo generate`-style) so a new extension is one
  command, not a copied example.
- Settle the Node/TypeScript bridge question: either finish it as a supported
  path or mark it explicitly experimental and stop advertising it.

### 5. Startup initialisation (measured)

The benchmark in [`docs/performance/benchmark-vs-pi.md`](docs/performance/benchmark-vs-pi.md) puts ki
at **17.7 ms** from spawn to cold start (native Pi: 189.5 ms). Core startup is
essentially just process creation — config, session store, tool registry and the
harness are negligible (ki's `--version` and its cold start differ by 0.2 ms).

The one real startup cost is **extension loading**, and it scales with what the user
installed: the same measurement reads ~100 ms on a machine whose
`~/.ki/agent/extensions` holds 25 installed wasm extensions. So the work here is:

- make extension discovery and loading lazy (or parallel) so startup does not
  scale with the number of installed extensions;
- keep the RPC `ready` marker honest — it must keep meaning "commands are being
  accepted", not "the work was deferred somewhere later".

Reproduce with `node scripts/bench-vs-pi.mjs`. Set **both** tools' isolation
variables (`KI_*` for ki, `PI_*` for pi) or the ki number will include your
global extensions.

Still unmeasured and worth adding: tool-loop throughput, memory growth over a
long session, compaction cost, and interactive TUI frame cost.

## Later

- **Sandboxing.** Extension code and `bash` currently run with the user's full
  privileges by design. A documented sandbox profile (seccomp/AppContainer or
  a container runner) would make the agent safer to point at untrusted
  repositories. This is a design problem, not a patch.
- **Multi-agent orchestration in-tree.** Sub-agents exist as an out-of-tree
  extension. The question is whether the runtime should own delegation,
  hand-off and budget accounting.
- **Editor/IDE integrations** built on the remote-mode protocol, so the agent
  is usable outside the terminal.
- **Windows parity** for anything currently POSIX-shaped in the tool layer.
- **Structured output and evaluation tooling** for teams that want to run the
  agent in CI.

## Not planned

- A hosted service. `ki` is a library and a local tool; there is no plan to
  run inference or store user sessions on project infrastructure.
- A visual/GUI application. The TUI and the JSONL remote protocol are the
  supported interfaces.
- Bundling a model. Providers are BYO-key or BYO-endpoint.

## How to influence this

Open an issue describing the problem you hit, not just the feature you want.
Features that unblock a real workflow move up; features that only add surface
area move down. If you want to work on something listed here, comment on the
issue first so we can agree on the shape before you write the patch. See
[CONTRIBUTING.md](CONTRIBUTING.md).
