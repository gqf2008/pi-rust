# kiqi-conformance

A minimal kiqi **wasm extension** used as the conformance fixture for the wasm
tier. It is not a real extension — it exercises the ABI surfaces the host must get
right (tools, streaming progress, logging, a denied capability).

ABI 0.8.0: `continue_command` and the `oauth-api` / `provider-api` / `render-api` exports are mandatory on every guest; this fixture implements all of them by hand to exercise the surfaces.

Build:

```sh
cargo build --manifest-path extensions/wasm/kiqi-conformance/Cargo.toml \
  --target wasm32-wasip2 --release
```

The output is a WebAssembly **component** at
`extensions/wasm/target/wasm32-wasip2/release/kiqi_conformance.wasm`.
