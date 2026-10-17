# kiqi-permissions

Permission policy storage for tool-call workflows. Rules use `Tool(pattern)`
entries in `.ki/permissions.json` (project-local; the legacy `.pi/` location
still works) or `~/.ki/agent/permissions.json` (global fallback; legacy
`~/.pi/agent/` still works). `deny` rules are evaluated first; when an allow
list exists for a tool, unmatched calls are denied.

> **Host-owned since the permissions-host batch.** The `ki` host now loads,
> evaluates and enforces this policy itself (`crates/kiqi-cli/src/permissions.rs`)
> and registers its own `permissions` tool. This crate is no longer in the
> default build; it stays available behind `--features plugin-kiqi-permissions`
> for one release. When both are enabled the compiled plugin's tool replaces
> the host's same-named built-in, but the host's `before_tool_call` gate always
> runs first and cannot be widened by a plugin.

Use `permissions` with `check`, `grant`, `revoke`, and `list` actions.
