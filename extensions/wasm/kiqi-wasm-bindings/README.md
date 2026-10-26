# kiqi-wasm-bindings

Guest SDK for writing **kiqi wasm extensions** (component model, ABI 0.8.0).

Wraps `wit_bindgen::generate!` over
[`extensions/kiqi-extension.wit`](../../kiqi-extension.wit). An extension crate implements the
generated `Guest` trait and calls `bindings::export!` — see
[`kiqi-conformance`](../kiqi-conformance) for a worked example.

ABI 0.8.0 makes `continue_command` and the `oauth-api` / `provider-api` / `render-api`
exports mandatory on every guest; `deny_contribution_exports!` implements the three
contribution traits as empty/denied stubs for an extension that contributes none of them.

Build an extension with:

```sh
cargo build --manifest-path extensions/wasm/kiqi-conformance/Cargo.toml \
  --target wasm32-wasip2 --release
```
