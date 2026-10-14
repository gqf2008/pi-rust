# kiqi-todo-guest

The first **real** kiqi wasm plugin: `rpi-todo`, ported off the compile-time
`trait Plugin` onto the 0.3.0 WIT. It is the reference port the plan calls for —
it exercises tools, the structured `tool-result.details` state channel, and
`session-read` (restoring the task list from the session branch).

Build:

```sh
cargo build --manifest-path plugins/wasm/kiqi-todo-guest/Cargo.toml \
  --target wasm32-wasip2 --release
```
