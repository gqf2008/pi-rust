# kiqi-plugin-guest

Guest SDK for writing **kiqi wasm plugins** (component model, ABI 0.8.0).

Wraps `wit_bindgen::generate!` over
[`plugins/kiqi-plugin.wit`](../../kiqi-plugin.wit). A plugin crate implements the
generated `Guest` trait and calls `bindings::export!` — see
[`kiqi-conformance-guest`](../kiqi-conformance-guest) for a worked example.

ABI 0.8.0 makes `continue_command` and the `oauth-api` / `provider-api` / `render-api`
exports mandatory on every guest; `deny_contribution_exports!` implements the three
contribution traits as empty/denied stubs for a plugin that contributes none of them.

Build a plugin with:

```sh
cargo build --manifest-path plugins/wasm/kiqi-conformance-guest/Cargo.toml \
  --target wasm32-wasip2 --release
```
