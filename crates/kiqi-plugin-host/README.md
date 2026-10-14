# kiqi-plugin-host

Host-side backend for **wasm plugins** — the component-model tier.

Binds [`plugins/kiqi-plugin.wit`](../../plugins/kiqi-plugin.wit) (ABI 0.6.0)
with wasmtime's `bindgen!` and hosts a plugin component behind the same
`kiqi_plugin::Plugin` seam the compiled-in plugins implement.
