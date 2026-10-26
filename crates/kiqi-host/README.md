# kiqi-host

ki's host-side plugin crate: the `Extension` contract, the event dispatch
spine, and the wasm (component model) plugin backend — formerly three crates
(`kiqi-plugin` + `kiqi-plugin-host` + `kiqi-extensions`), merged 2026-10-26
(see `docs/design/kiqi-host-merge.md`).

## Layers

| Layer | Gate | Contents |
|---|---|---|
| Contract | always | the [`Extension`] trait every plugin backend implements, `EventTag` / `Event` / `EventAction` (37 tags), host identity + env-var constants, `grants`, `dialogs` |
| Dispatch spine | always | `registry`, `translate` funnels, the `compiled` sink that fans events into every wasm host, action bridge + UI mailboxes, stdio lifecycle, the C-ABI bus types in `abi` (internal wire format) |
| Wasm backend | `wasm` feature | `wasm` module: wasmtime `bindgen!` over `plugins/kiqi-plugin.wit` (ABI 0.8.0), per-grant capability gates (oauth / provider / render / resources / provider-request), sandboxed guest execution |

## Red line

Everything outside the `wasm` module must build **without** wasmtime:
`kiqi-tools`, `kiqi-mcp` and `kiqi-acp` depend on this crate unconditionally so
they can use the constants and `EventTag`; only `kiqi-cli` (the composition
root) enables the `wasm` feature.

## Wasm backend notes

Binds `plugins/kiqi-plugin.wit` with wasmtime's `bindgen!` and hosts a plugin
component behind the same `Extension` seam. The cdylib channel that used to
provide a second backend was retired on 2026-10-26 — wasm is the only plugin
channel.

End-to-end coverage lives in `tests/e2e.rs` (gated on the `wasm` feature; it
builds the conformance guest, so it needs `rustup target add wasm32-wasip2`).
