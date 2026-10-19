# kiqi-cli

[![crates.io](https://img.shields.io/crates/v/kiqi-cli.svg)](https://crates.io/crates/kiqi-cli)
[![docs.rs](https://docs.rs/kiqi-cli/badge.svg)](https://docs.rs/kiqi-cli)
[![CI](https://github.com/bigfish1913/pi-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/bigfish1913/pi-rust/actions)

The terminal coding-agent CLI — the `ki` binary — built on the `kiqi-*` library
crates. It is the ready-to-run surface of the
[ki](https://github.com/bigfish1913/pi-rust) project: an interactive TUI, a
one-shot `-p` mode, a JSON event mode, durable sessions, and Rust `cdylib`
plugin loading.

## Install

```bash
cargo install kiqi-cli
```

Requires Rust 1.96 or newer. Then point it at a provider and start it:

```bash
export ANTHROPIC_API_KEY=...     # or OPENAI_API_KEY, or ~/.ki/agent/models.json
ki
```

One-shot, and machine-readable output:

```bash
ki -p "summarize the README"
ki --mode json -p "list the crates"
```

## Authentication

Credentials are resolved in priority order:

1. `--api-key` on the command line
2. `~/.ki/auth.json`, written by `ki auth login` (mode `0o600` on Unix)
3. the `apiKey` field of a provider in `~/.ki/agent/models.json`
4. provider environment variables: `OPENAI_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
   `ANTHROPIC_API_KEY`

`~/.ki/` can be relocated with `KI_CODING_AGENT_DIR`.

## Custom providers and gateways

Any Anthropic-Messages or OpenAI-compatible endpoint can be declared in
`~/.ki/agent/models.json`:

```jsonc
{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "sk-gateway-secret",
      "authHeader": true,
      "headers": { "x-portkey-key": "…" },
      "models": [{ "id": "custom-claude", "name": "Custom Claude" }]
    }
  }
}
```

```bash
ki --model gateway/custom-claude -p "hi"
```

Use `"api": "openai-completions"` for an OpenAI-compatible endpoint. Anthropic
endpoint overrides also honour `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`.

## Sessions and resources

- Sessions are JSONL v4 files under the session directory, with branching and
  context compaction.
- Project resources are discovered from `.ki/` (skills, prompts, themes, and
  `SYSTEM.md`), with `~/.ki/agent/` as the global layer. When both define the
  same skill or prompt name, the project copy wins.
- Plugins are loaded from project `.ki/extensions`, global
  `~/.ki/agent/extensions`, and `--extensions-dir`.

## Plugins

```bash
ki install kiqi-extension-example              # from crates.io
ki install kiqi-extension-example --version 0.1.0
ki install my-extension --path ../my-extension --force   # local
ki dev                                        # build + watch a local extension
```

`ki install` builds the crate in release mode with Cargo and copies the
resulting `.dll` / `.so` / `.dylib` into `~/.ki/agent/extensions`.

## Remote mode

```bash
ki --server                      # headless agent over TCP, prints a token
ki --connect host:port --token T # zero-local-resource TUI client
```

The client holds no local provider, tools, extensions or session files; all
agent work happens on the server. See
[docs/remote/user-guide.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/remote/user-guide.md).

## Default tools

The CLI loads the Pi-compatible `read`, `write`, `edit` and `bash` tools, the
read-only `docs` lookup, and the host-owned `permissions` policy tool.
Additional implementations (`grep`, `find`, `ls`, `powershell`) remain
available in the [`kiqi-tools`](https://crates.io/crates/kiqi-tools) library and
through the `default_tools` setting.

## Library usage

The crate also exposes its internals, so the CLI surface can be embedded:

```toml
[dependencies]
kiqi-cli = "0.3"
```

For building your own agent, depend on
[`kiqi-agent`](https://crates.io/crates/kiqi-agent) and
[`kiqi-harness`](https://crates.io/crates/kiqi-harness) directly instead — see
[docs/agent/project-structure.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/agent/project-structure.md).

## Documentation

- User guide: <https://ki.laofu.online/docs.html>
- Architecture: [docs/architecture/overview.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture/overview.md)
- Extension authoring: [docs/extensions/authoring.md](https://github.com/bigfish1913/pi-rust/blob/main/docs/extensions/authoring.md)
- Changelog: [CHANGELOG.md](https://github.com/bigfish1913/pi-rust/blob/main/CHANGELOG.md)

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).

This crate is part of a Rust port of the MIT-licensed TypeScript `pi` SDK
(© Mario Zechner).
