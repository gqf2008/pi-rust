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
