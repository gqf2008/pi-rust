# kiqi-todo

The first **real** kiqi wasm extension: `kiqi-todo`, ported off the compile-time
`trait Extension` onto the 0.3.0 WIT. It is the reference port the plan calls for —
it exercises tools, the structured `tool-result.details` state channel, and
`session-read` (restoring the task list from the session branch).

Build:

```sh
cargo build --manifest-path extensions/wasm/kiqi-todo/Cargo.toml \
  --target wasm32-wasip2 --release
```

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_wasm_bindings::deny_contribution_exports!(…)`.
