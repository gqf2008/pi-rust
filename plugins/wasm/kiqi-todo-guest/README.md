# kiqi-todo-guest

The first **real** kiqi wasm plugin: `kiqi-todo`, ported off the compile-time
`trait Plugin` onto the 0.3.0 WIT. It is the reference port the plan calls for —
it exercises tools, the structured `tool-result.details` state channel, and
`session-read` (restoring the task list from the session branch).

Build:

```sh
cargo build --manifest-path plugins/wasm/kiqi-todo-guest/Cargo.toml \
  --target wasm32-wasip2 --release
```

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_plugin_guest::deny_contribution_exports!(…)`.
