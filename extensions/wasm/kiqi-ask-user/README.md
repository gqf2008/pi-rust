# kiqi-ask-user

`kiqi-ask-user` ported to wasm. It is the reference case for the `ui` capability:
it normalizes the tool arguments, opens one host dialog per question, polls it
to an answer, and honours cancellation and `timeout`.

The compile-time extension needs a start/poll/cancel state machine only because a
C ABI cannot block; here `call_tool` simply blocks, so the same interaction is a
straight loop.

Build:

```sh
cargo build --manifest-path extensions/wasm/kiqi-ask-user/Cargo.toml \
  --target wasm32-wasip2 --release
```

ABI 0.8.0: a rebuild must also implement `continue_command` and the `oauth-api` / `provider-api` / `render-api` contribution exports; this crate fills the latter three with `kiqi_wasm_bindings::deny_contribution_exports!(…)`.
