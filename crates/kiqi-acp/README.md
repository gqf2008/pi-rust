# kiqi-acp

The Agent Client Protocol server behind `ki acp serve`: an editor launches `ki`
as an external agent and drives it over ACP on stdio.

## Why this is a crate and not an extension

A protocol server has to own the process's stdio and stay alive across many
turns and sessions — it cannot live inside a sandbox that is loaded per session.
So `ki acp serve` claims argv/stdin/stdout before the CLI resolves a provider,
and this crate implements the protocol on top of the same session machinery the
interactive agent uses. The design is in `docs/design/ki-acp-serve.md`.

## What it implements

- `initialize` — protocol negotiation plus a snapshot of the client's capabilities.
- `session/new`, `session/load`, `session/close` — several live sessions per
  process; `session/load` replays a stored session's history.
- `session/prompt` — one turn, streamed as `session/update` deltas.
- `session/cancel` — arms only while a turn is in flight, trips that turn's
  token, and answers with the turn's real outcome.

## Behaviour you can rely on

- **stdout carries only the protocol.** Every human-facing line goes to stderr.
- **Dispatch is serial**, so the one long-running method (`session/prompt`)
  moves its turn into a spawned task; a failing turn reports through the
  responder instead of tearing the connection down.
- **A turn drains to the terminal `AgentEnd` before responding**, so a response
  never overtakes the last `session/update`.
- **Sessions are independent**: one session's in-flight turn does not block
  another session from being created or run.

## Install

The crates are not published to crates.io; depend on them from this repository.

```toml
[dependencies]
kiqi-acp = { git = "https://github.com/gqf2008/pi-rust" }
```

## Layout

- `src/lib.rs` — the connection: negotiation, routing, the outgoing actor.
- `src/sessions.rs` — one `LiveSession` per ACP session, turn state, cancellation.
- `src/projection.rs` — pure mapping from ki's agent events to `session/update`
  payloads (deltas only; no new wire behaviour beyond the documented mapping).
- `src/dialogs.rs` — permission and dialog requests routed to the client.
- `src/zed.rs` — `ki acp install-zed`: splices a single member into Zed's JSONC
  settings, leaving comments, formatting and every other byte untouched.

## Related crates

| Crate | Role |
| ----- | ---- |
| [`kiqi-harness`](../kiqi-harness) | Sessions, persistence and the run loop this server drives |
| [`kiqi-agent`](../kiqi-agent) | The event stream projected onto `session/update` |
| [`kiqi-host`](../kiqi-host) | The dialog and permission contract shared with extensions |
| [`kiqi-cli`](../kiqi-cli) | The binary that owns the `acp serve` and `acp install-zed` subcommands |

## Documentation

- Crate docs: `cargo doc --open -p kiqi-acp`
- Design: `docs/design/ki-acp-serve.md`

## License

MIT. See [LICENSE](../../LICENSE) and [NOTICE](../../NOTICE).
