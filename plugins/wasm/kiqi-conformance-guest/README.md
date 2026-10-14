# kiqi-conformance-guest

A minimal kiqi **wasm plugin** used as the conformance fixture for the wasm
tier. It is not a real plugin — it exercises the ABI surfaces the host must get
right (tools, streaming progress, logging, a denied capability).

Build:

```sh
cargo build --manifest-path plugins/wasm/kiqi-conformance-guest/Cargo.toml \
  --target wasm32-wasip2 --release
```

The output is a WebAssembly **component** at
`plugins/wasm/target/wasm32-wasip2/release/kiqi_conformance_guest.wasm`.
