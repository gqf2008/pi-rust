//! Host-side backend for **wasm plugins** (the component model tier).
//!
//! This crate binds `plugins/kiqi-plugin.wit` (ABI 0.2.0) into Rust with
//! wasmtime's `bindgen!` and will host a plugin component behind the same
//! [`kiqi_plugin::Plugin`] seam the compiled-in plugins implement. Keeping the
//! seam sync is deliberate: the async lives here (driving wasmtime's async
//! component calls and fulfilling the host imports), not in the plugin trait.
//!
//! The first landed piece is the binding itself — it proves the WIT and the
//! generated host trait shapes before any of the runtime plumbing is written.

wasmtime::component::bindgen!({
    path: "../../plugins/kiqi-plugin.wit",
    world: "plugin",
});
