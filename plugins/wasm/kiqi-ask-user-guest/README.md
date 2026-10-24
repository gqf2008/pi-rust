# kiqi-ask-user-guest

`kiqi-ask-user` ported to wasm. It is the reference case for the `ui` capability:
it normalizes the tool arguments, opens one host dialog per question, polls it
to an answer, and honours cancellation and `timeout`.

The compile-time plugin needs a start/poll/cancel state machine only because a
C ABI cannot block; here `call_tool` simply blocks, so the same interaction is a
straight loop.

Build:

```sh
cargo build --manifest-path plugins/wasm/kiqi-ask-user-guest/Cargo.toml \
  --target wasm32-wasip2 --release
```

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_plugin_guest::deny_contribution_exports!(…)`.
