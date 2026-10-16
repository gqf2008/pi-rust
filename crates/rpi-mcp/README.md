# rpi-mcp

Host-level MCP (Model Context Protocol) client for `ki`, the Rust coding agent.

MCP is a **host capability here, not a plugin**: external servers are declared in
ki's own `settings.json` under `mcpServers`, the connection lives with the
session, and each discovered tool is expanded into a native ki tool named
`{server}__{tool}` so the model sees it directly.

Batch 1 is stdio only (no resources, no HTTP/SSE — those are later batches).

## Config

```jsonc
// ~/.ki/agent/settings.json or <cwd>/.ki/settings.json
{
  "mcpServers": {
    "codegraph": { "command": "codegraph", "args": ["serve", "--mcp"],
                   "env": {}, "cwd": null, "enabled": true, "timeoutSeconds": 60 }
  }
}
```

The shape is reused verbatim from the retired `rpi-mcp-adapter`, so users' files
port unchanged (the adapter's dead `exposure` field is ignored, not modeled).
Global and project settings merge by name — a project entry overrides a global
one of the same name, and a project's servers are only honored when the project
is trusted.

## Trust

A server is a process ki spawns and (in later batches) the network it reaches.
Authorization is the user's own settings entry. Every spawn clears the
environment and re-adds only an allowlist (`PATH`, `HOME`, `LANG`/`LC_*`,
`TMPDIR`, `TERM`, `USER`, `SHELL`), so a server never inherits ki's provider API
keys. Every spawn and every skip/failure is logged to stderr. Shutdown reaps the
whole process tree (process group on unix, job object on windows).

## Layout

* `src/config.rs` — schema, validation, global/project merge.
* `src/manager.rs` — connect, handshake, `tools/list`, shutdown.
* `src/tool.rs` — the `AgentTool` adapter and MCP → ki content mapping.
* `tests/` — an offline end-to-end run against a fake stdio server
  (`tests/support/fake_server.rs`).
