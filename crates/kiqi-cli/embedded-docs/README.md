# ki — a Rust-native coding-agent runtime

[![license](https://img.shields.io/crates/l/kiqi-cli.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.96-blue.svg)](#install)
[![CI](https://github.com/gqf2008/pi-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/gqf2008/pi-rust/actions/workflows/ci.yml)
[![GitHub stars](https://img.shields.io/github/stars/gqf2008/pi-rust?style=flat)](https://github.com/gqf2008/pi-rust/stargazers)
[![Latest release](https://img.shields.io/github/v/release/gqf2008/pi-rust)](https://github.com/gqf2008/pi-rust/releases/latest)

`ki` is a library-first coding-agent runtime written in Rust, plus a terminal
agent built on top of it. Nine composable crates span provider adapters, a
durable crash-resumable agent loop, a terminal UI, and a stable plugin ABI — so
the same code works as an embedded SDK or as a ready-to-run `ki` command.

Website: <https://ki.laofu.online/> · Docs: <https://ki.laofu.online/docs.html>

![ki interactive session](docs/images/kiqi-interactive.png)

<sub>Interactive TUI. See [`docs/guides/user-guide.md`](docs/guides/user-guide.md) for the full command surface.</sub>

![ki SDK demo — an agent loop and a real tool round-trip, offline with the faux provider](docs/images/kiqi-sdk-demo.gif)

<sub>The agent loop and a real `write` tool round-trip, recorded with no API key and no network — the same path the tests take. Source of truth is [`demo/sdk.tape`](demo/sdk.tape); regenerate with `task demo`.</sub>

## Three ways to use it

| You want | Depend on | Start at |
| -------- | --------- | -------- |
| A working terminal coding agent | nothing | `scripts/install.sh` (see below) |
| Your own agent inside your process | `kiqi-agent` (+ `kiqi-ai`, `kiqi-tools`) | [`examples/minimal`](examples/minimal) |
| A tool/provider that extends every session | `kiqi-plugin-sdk` | [`examples/plugin-stub`](examples/plugin-stub) |

The three share one implementation. Build your tools once, debug them inside an
embedded agent, and ship the same code as a `cdylib` plugin that the global `ki`
loads. See [`docs/agent/project-structure.md`](docs/agent/project-structure.md) for the recommended
project layout.

## Install

**Prebuilt binary** (no Rust toolchain needed):

```bash
curl -fsSL https://raw.githubusercontent.com/gqf2008/pi-rust/main/scripts/install.sh | sh
```

Verifies the published SHA-256 checksum and installs to `/usr/local/bin` or
`~/.local/bin`. Preview first with `--dry-run`.

**From source** (requires Rust 1.96+):

```bash
git clone https://github.com/gqf2008/pi-rust && cd pi-rust
cargo install --path crates/kiqi-cli --force
ki --version
```

The crates are not published to crates.io, so `cargo install kiqi-cli` does not work —
it would fetch a different (or no) package.

**Package managers:** not available yet. `gqf2008/homebrew-tap` and
`gqf2008/scoop-bucket` do not exist, and the `bigfish1913` taps belong to the
unrelated fork — they carry that project's `rpi` formula/manifest, not `ki` — so
`brew install ki` / `scoop install ki` have nothing to install. `packaging/` holds
a formula and manifests that have never been published. The open winget PRs
(`#441442`, `#444745`) are for the other project's `bigfish1913.rpi`, not `ki`.

Use the prebuilt installer or a source build until then:

**From source:**

```bash
git clone https://github.com/gqf2008/pi-rust.git
cd pi-rust && cargo run -p minimal   # offline agent, no API key required
```

Real output of the two offline examples — no credentials, no network, identical
bytes every run. Regenerate with `task demo-transcript`:

```text
$ cargo run -q -p minimal
minimal example: 2 messages after run
  - user
  - assistant
assistant reply: Hello from the faux provider!
AgentEnd observed.

$ cargo run -q -p tools-example
tools example: 4 messages after run
wrote out.txt (28 bytes): "hello from the tools example"
AgentEnd observed; tool ran against OsExecutionEnv.
```

## Quick start

### The CLI

```bash
export ANTHROPIC_API_KEY=...          # or OPENAI_API_KEY, or ~/.ki/agent/models.json
ki                                   # interactive TUI
ki -p "summarize the README"         # one-shot
ki --mode json -p "list the crates"  # machine-readable event stream
```

Sessions persist as JSONL and survive a crash mid-run; `ki` resumes from the
last committed frame rather than replaying the turn. Custom Anthropic or
OpenAI-compatible gateways are declared in `~/.ki/agent/models.json` and
selected with `--model <provider>/<model>`.

### The SDK

Build an agent against a deterministic in-process provider — no API key, no
network, no flakiness:

```rust
use kiqi_agent::AgentBuilder;
use kiqi_ai::providers::faux::{FauxProvider, FauxScript};
use kiqi_ai::Provider;

#[tokio::main]
async fn main() {
    // `FauxProvider::new` already returns an `Arc<Self>`.
    let provider = FauxProvider::new(FauxScript::new().with_text("Hello from the faux provider!"));
    let model = provider.default_model().clone();

    let agent = AgentBuilder::new()
        .model(model)
        .system_prompt("You are a helpful assistant.")
        .stream_fn(make_stream_fn(provider))
        .build()
        .expect("agent builds");

    let mut events = agent.subscribe();
    agent.prompt("hi").await.expect("prompt completes");

    while let Ok(ev) = events.try_recv() {
        let _ = ev; // AgentEvent::{MessageUpdate, ToolEnd, AgentEnd, …}
    }
    println!("{} messages after the run", agent.state().messages.len());
}
```

`make_stream_fn` bridges the synchronous `StreamFn` contract to the async
provider — the full version is in [`examples/minimal/src/main.rs`](examples/minimal/src/main.rs)
(`cargo run -p minimal`).

## Crates

Nine crates, one version, published together. Dependency direction is one-way
and enforced in review:

```
kiqi-telemetry → kiqi-ai → kiqi-agent → kiqi-tools → kiqi-harness
```

Three crates sit outside that chain:

- `kiqi-tui` depends on no other `kiqi-*` crate.
- `kiqi-plugin-sdk` is a leaf too (it has no `kiqi-*` deps), but it is not only
  the host's plugin contract: `kiqi-tools` also reads its env-var names, so the
  host and its plugins cannot disagree about the strings they exchange across
  the ABI. That edge is names-only — nothing is linked or called.
- `kiqi-extensions` is `kiqi-plugin-sdk` plus `kiqi-ai`/`kiqi-agent`, since it hosts
  plugins that run tools.
- `kiqi-cli` depends on all eight of the others, which is how the `ki` binary
gets the TUI, the harness and the plugin loader at once.

| Crate | What it is |
| ----- | ---------- |
| [`kiqi-telemetry`](./crates/kiqi-telemetry) | Span/event contracts with a no-op default — instrumentation is opt-in, and pulls in no tracing or OpenTelemetry backend |
| [`kiqi-ai`](./crates/kiqi-ai) | Provider-agnostic message, streaming and tool-schema types; Anthropic + OpenAI-compatible + `faux` providers |
| [`kiqi-agent`](./crates/kiqi-agent) | The agent loop: `AgentTool`, events, hooks, queues, cancellation |
| [`kiqi-tools`](./crates/kiqi-tools) | `read`/`write`/`edit`/`bash` (+ `grep`, `find`, `ls`, `powershell`) and the `ExecutionEnv` seam |
| [`kiqi-harness`](./crates/kiqi-harness) | Session tree, JSONL persistence, compaction, crash recovery, the run loop |
| [`kiqi-tui`](./crates/kiqi-tui) | Terminal UI primitives: components, layout, editor, transcript rendering |
| [`kiqi-cli`](./crates/kiqi-cli) | The `ki` binary: TUI, one-shot and JSON modes, config, plugin loading |
| [`kiqi-plugin-sdk`](./crates/kiqi-plugin-sdk) | The stable `#[repr(C)]` ABI contract for plugins. No `kiqi-*` deps; only `serde_json`, behind a default `json` feature that can be turned off |
| [`kiqi-extensions`](./crates/kiqi-extensions) | Host-side plugin loader and the async-across-ABI tool bridge |

Every crate has its own README. The crates are **not** published to crates.io
(`publish = false` in each manifest), so links above point at the in-repo
directories; `docs.rs` has no pages for them.

## Why ki

Most coding agents are CLIs. `ki` is a **runtime with a CLI attached**, which
changes what you can build with it:

- **Embeddable.** The agent loop is a library. `kiqi-agent` + `kiqi-ai` is enough
  to run an agent inside your own process, with your own event handling and
  output — no shelling out to a tool and scraping stdout.
- **Testable offline.** A deterministic `faux` provider and an in-memory
  execution environment are first-class, not mocks bolted on. `kiqi-ai` puts its
  real HTTP providers behind a `providers` feature flag, so your test suite
  cannot silently depend on the network for the paths you exercise.
- **Durable.** Sessions are a tree with JSONL persistence and per-frame
  progress. A crash mid-tool-call resumes instead of losing the turn or
  double-executing the tool.
- **Extensible across a stable boundary.** Plugins are Rust `cdylib`s behind a
  hand-written, panic-contained C ABI with version negotiation — the ABI
  survives compiler and crate version skew, so extensions do not break on every
  release.
- **One implementation, two surfaces.** The global CLI and an embedded agent load
  the same tools. You do not maintain a plugin and a library variant separately.

### How it compares

Structural differences against other terminal coding agents. Facts about other
tools are as of 2026-09 and change often — check their documentation; only the
`ki` column is maintained here.

| | ki | Claude Code | Codex CLI | opencode | aider |
| --- | --- | --- | --- | --- | --- |
| Language | Rust | TypeScript | Rust | TypeScript/Go | Python |
| Source available | ✅ MIT | ❌ | ✅ | ✅ | ✅ |
| Usable as an embedded library | ✅ | ❌ | ❌ | ⚠️ | ⚠️ |
| First-class plugin ABI | ✅ | hooks/MCP | ❌ | plugin API | ❌ |
| Headless server + remote client | ✅ | ⚠️ | ✅ | ✅ | ⚠️ |
| Provider-agnostic gateways | ✅ | ❌ | ❌ | ✅ | ✅ |

If you want the most polished single-vendor experience, use the vendor's tool.
`ki` is for people who want to own the loop.

## Performance

Measured on Windows 11 (x86_64), rustc 1.97.1, `--release`. Both scripts
rebuild and re-measure on your own machine.

`sh scripts/bench.sh` — standalone numbers:

| Metric | Value |
| ------ | ----- |
| Release binary size | **21.8 MiB** |
| `ki --version` wall time | **~17 ms** median |
| RSS once the agent is ready | **12.0 MiB** |

`node scripts/bench-vs-pi.mjs --pi <path-to-pi>` — against native Pi on the same
machine, both offline, each with a fresh isolated config directory:

| Metric | ki (Rust) | pi (TypeScript) | Difference |
| ------ | ---------: | --------------: | ---------- |
| `--version` | **17.5 ms** | 169.5 ms | **9.7× faster** |
| Cold start | **17.7 ms** | 189.5 ms | **10.7× faster** |
| RSS at ready | **12.0 MiB** | 91.5 MiB | **7.6× smaller** |
| Install footprint | **21.8 MiB** (one binary) | ~385 MiB (+ ~91 MiB Node) | **~18–22× smaller** |

**Cold start** is spawn → the agent answering its first command: process creation
plus full initialisation, excluding teardown and any LLM work. ki's `--version`
(17.5 ms) and cold start (17.7 ms) are effectively equal, so startup is process
creation, not initialisation; Pi is dominated by Node boot.

Isolation caveat: ki reads `KI_CODING_AGENT_DIR` / `KI_OFFLINE`, native Pi reads
`PI_CODING_AGENT_DIR` / `PI_OFFLINE`. Set the matching pair for each, or ki
silently loads the machine's global plugins.

Method, raw per-run numbers, and what is *not* measured (LLM latency, tool-loop
throughput, long sessions, TUI frame cost) are in
[`docs/performance/benchmark-vs-pi.md`](docs/performance/benchmark-vs-pi.md).

## Plugins

Extensions arrive through two channels.

**The capabilities that ship with this repository are wasm components.** Each is
a WebAssembly component (`plugins/kiqi-plugin.wit` is the contract) running in a
wasmtime sandbox, granted only the capabilities its install approves:

```bash
rustup target add wasm32-wasip2                   # once, to build guests
ki insmod --build plugins/wasm/kiqi-todo-guest    # build that guest, then install it
ki insmod path/to/component.wasm --yes            # install an already-built component
```

[`website/data/packages.json`](website/data/packages.json) lists which
capabilities are available this way, with the exact command for each.

**Third-party extensions are Rust `cdylib` libraries** behind a stable
`#[repr(C)]` ABI. An extension can register tools, providers, slash commands,
event handlers, markdown renderers and discovered resources.

```bash
ki install my-ext --path ../my-ext     # local, for development
ki dev                                 # build + watch + live reload
```

No crate in this repository is published to crates.io, so `ki install <name>`
without `--path` has nothing to resolve — build from source. The host loads
project `.ki/extensions`, then global `~/.ki/agent/extensions`, then any
`--extensions-dir`.

Ready-to-install third-party extensions live in the companion
[`pi-rust/rpi-package`](https://github.com/pi-rust/rpi-package) repository, with
an [online catalog](https://ki.laofu.online/packages.html).

```rust
use kiqi_plugin_sdk::{export_plugin, StbStringRef};

export_plugin!(|api| {
    if let Some(declare) = api.declare {
        declare(StbStringRef::from_str(r#"{"priority":60,"platforms":["linux"]}"#));
    }
    // register tools / handlers through `api`
    0
});
```

Start from [`examples/plugin-stub`](examples/plugin-stub) — a complete plugin
with the full `execute`→`poll`→`cancel`→`destroy` lifecycle — and read
[`docs/extensions/authoring.md`](docs/extensions/authoring.md) for the safety
rules, which are load-bearing rather than stylistic.

## Remote mode

```bash
ki --server                        # headless agent over TCP, prints an auth token
ki --connect host:port --token T   # TUI client with zero local agent resources
```

The client holds no provider, tools, extensions or session files — all agent
work happens on the server, so a thin terminal anywhere can drive a beefy
machine. See [`docs/remote/user-guide.md`](docs/remote/user-guide.md).

## Project status

| | |
| --- | --- |
| Current release | **[0.4.1](https://github.com/gqf2008/pi-rust/releases/tag/v0.4.1)** (all crates share one workspace version) |
| Stability | `kiqi-ai`, `kiqi-agent`, `kiqi-tools`, `kiqi-harness`, `kiqi-plugin-sdk` are the intended stable surface |
| MSRV | 1.96 |
| Platforms | Linux, macOS, Windows (CI runs the suite on Linux) |
| Binary release targets | Linux (x86_64/aarch64), macOS (Intel/Apple silicon), Windows (x86_64) |

`ki` is a self-contained Rust coding-agent stack. No Node component is
required and none is loaded.

## Documentation

- [User guide](docs/guides/user-guide.md) — commands, config, sessions, skills
- [Architecture](docs/architecture/overview.md) — how the crates fit together and why
- [Building an agent project](docs/agent/project-structure.md) — recommended structure
- [Extension authoring](docs/extensions/authoring.md) — plugin templates and ABI rules
- [Remote mode](docs/remote/user-guide.md) — protocol, tokens, limitations
- [Roadmap](ROADMAP.md) · [Changelog](CHANGELOG.md)
- [docs.rs](https://docs.rs/kiqi-cli) for per-crate API docs

## Contributing

Issues and PRs are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md) for the
build/test commands and the repository layout, and the
[roadmap](ROADMAP.md) for what is already planned. Security problems should go
through [SECURITY.md](SECURITY.md), not a public issue.

The whole test suite runs offline: `cargo test --workspace --locked`.

## Community

Questions, ideas and early builds are welcome in either group:

<table>
  <tr>
    <td align="center" width="50%">
      <img src="docs/images/weixin.jpg" width="230" alt="WeChat group QR code"><br>
      <sub><b>WeChat</b> — <a href="https://weixin.qq.com/g/AQYAAD8z3FmKPk143hi_HUVOtGvNqlHWMtVZu1VGGB_46JKoNpyhJvS7kDwQwUTr">微信交流群</a></sub>
    </td>
    <td align="center" width="50%">
      <img src="docs/images/tg.png" width="230" alt="Telegram group QR code"><br>
      <sub><b>Telegram</b> — <a href="https://t.me/+7YfN4TZPP7Y0M2Q1">t.me/+7YfN4TZPP7Y0M2Q1</a></sub>
    </td>
  </tr>
</table>

<sub>Group invite codes expire — WeChat's rotates every few days. If a QR code stops working, open an issue and it will be refreshed.</sub>

## License

MIT. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

The `kiqi-*` crates are a Rust-native coding-agent stack.

## Star history

[![Star History Chart](https://api.star-history.com/svg?repos=gqf2008/pi-rust,pi-rust/rpi-package&type=Date)](https://www.star-history.com/#gqf2008/pi-rust&pi-rust/rpi-package&Date)
