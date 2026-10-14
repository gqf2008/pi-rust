# kiqi-plugin-guest

Guest SDK for writing **kiqi wasm plugins** (component model, ABI 0.2.0).

Wraps `wit_bindgen::generate!` over
[`plugins/kiqi-plugin.wit`](../../kiqi-plugin.wit). A plugin crate implements the
generated `Guest` trait and calls `bindings::export!` — see
[`kiqi-conformance-guest`](../kiqi-conformance-guest) for a worked example.

Build a plugin with:

```sh
cargo build --manifest-path plugins/wasm/kiqi-conformance-guest/Cargo.toml \
  --target wasm32-wasip2 --release
```
