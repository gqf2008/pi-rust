# kiqi-mcp

Host-level MCP (Model Context Protocol) client for `ki`, the Rust coding agent.

MCP is a **host capability here, not an extension**: external servers are declared in
ki's own `settings.json` under `mcpServers`, the connection lives with the
session, and each discovered tool is expanded into a native ki tool named
`{server}__{tool}` so the model sees it directly.

## Transports

* **stdio** — `command` / `args` / `env` / `cwd`. One child process per server,
  spawned in its own process group (unix) or job object (windows) so shutdown
  reaps the whole tree.
* **HTTP (Streamable HTTP)** — `url` / `headers`. rmcp's session/SSE worker
  drives a repo-owned reqwest 0.12 client (`src/http.rs`), so ki keeps a single
  reqwest major and the same proxy/idle-timeout policy as its model providers.
  The worker adopts `Mcp-Session-Id`, sends `MCP-Protocol-Version`, keeps a
  standalone `GET` SSE stream for server notifications, re-initializes once and
  replays the request on an expired session (HTTP 404), and `DELETE`s the
  session on teardown. Every response is capped at 16 MiB (the stdio per-frame
  cap) and redirects are followed only within one origin.

The legacy SSE transport is **not** implemented; ACP refuses that variant with
`-32602`.

## Config

```jsonc
// ~/.ki/agent/settings.json or <cwd>/.ki/settings.json
{
  "mcpServers": {
    "codegraph": { "command": "codegraph", "args": ["serve", "--mcp"],
                   "env": {}, "cwd": null, "enabled": true, "timeoutSeconds": 60 },
    "docs":      { "url": "https://example.test/mcp",
                   "headers": { "Authorization": "Bearer …" },
                   "enabled": true, "timeoutSeconds": 30,
                   "subscribe": ["file:///docs/a.md"],
                   "autoRead":  ["file:///docs/a.md"] }
  }
}
```

The shape is reused verbatim from the retired `kiqi-mcp-adapter`, so users' files
port unchanged (the adapter's dead `exposure` field is ignored, not modeled).
Global and project settings merge by name — a project entry overrides a global
one of the same name, and a project's servers are only honored when the project
is trusted.

* `subscribe` — durable uris subscribed right after the handshake. The model
  can also start/stop session-scoped watches at runtime through
  `{server}__watch_resource` (exposed when the server advertises
  `resources.subscribe`). Subscriptions are restored one-by-one after a
  reconnect; one rejected uri drops only that uri.
* `modelWatch` — default `true`; set `false` to hide the runtime watch verb for
  this server. ACP client-supplied servers always disable it. Model watches are
  capped at 32 and are record-only unless the same uri is in `subscribe`, so
  they cannot consume the configured wake budget; a model `stop` cannot remove
  a configured watch.
* `autoRead` — an opt-in list of uris whose updates are read at the session
  boundary and appended to the batched notice entry (bounded at 20k with the
  usual truncation marker). Every other uri keeps the light notice: server +
  uri only, content read on demand through `{server}__read_resource`.

## Resources

A server advertising resources contributes a read tool,
`{server}__read_resource`; when it also advertises `resources.subscribe`, it
contributes `{server}__watch_resource` (`start`/`stop`/`list`) as well.
`notifications/resources/updated` (stdio or the
HTTP standalone SSE stream) delivers a coalescing, bounded notice; the model
reads the content on demand unless the uri is listed in `autoRead`.

## Trust

A server is a process ki spawns and/or a host it contacts. Authorization is the
user's own settings entry (ACP client-supplied servers are the editor user's own
request, logged loudly). Every stdio spawn clears the environment and re-adds
only an allowlist (`PATH`, `HOME`, `LANG`/`LC_*`, `TMPDIR`, `TERM`, `USER`,
`SHELL`), so a server never inherits ki's provider API keys. Every start,
reconnect, skip and failure is logged to stderr. Shutdown reaps the whole child
process tree and the HTTP session.

## ACP

`session/new` and `session/load` accept `mcpServers` entries of the stdio and
HTTP variants; `initialize` advertises `mcpCapabilities.http = true` and
`sse = false`.

## Layout

* `src/config.rs` — schema, validation, global/project merge.
* `src/manager.rs` — connect, handshake, `tools/list`, subscriptions, notices,
  lifecycle.
* `src/http.rs` — the reqwest 0.12 `StreamableHttpClient` rmcp's worker drives.
* `src/tool.rs` — the `AgentTool` adapter and MCP → ki content mapping.
* `src/notice.rs` — the bounded, coalescing resource-update mailbox.
* `tests/` — an offline end-to-end run against a fake stdio server
  (`tests/support/fake_server.rs`) and an in-process HTTP peer
  (`tests/support/http_fake.rs`).
