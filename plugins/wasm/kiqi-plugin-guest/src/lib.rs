//! Guest SDK for writing **kiqi wasm plugins**.
//!
//! Wraps [`wit_bindgen::generate!`] over `plugins/kiqi-plugin.wit` (ABI 0.8.0)
//! so a plugin crate only has to implement the generated `Guest` trait and call
//! [`bindings::export!`]:
//!
//! ```ignore
//! use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
//!
//! struct MyPlugin;
//! impl Guest for MyPlugin { /* ... */ }
//! kiqi_plugin_guest::bindings::export!(MyPlugin with_types_in kiqi_plugin_guest::bindings);
//! ```
//!
//! The generated bindings live in [`bindings`] so `export!` can be invoked from
//! the plugin crate (`with_types_in kiqi_plugin_guest::bindings`).

pub mod bindings {
    wit_bindgen::generate!({
        path: "../../kiqi-plugin.wit",
        world: "plugin",
        // The plugin crate, not this SDK crate, implements the exported trait
        // and calls `export!`, so both the macro and the generated items must be
        // reachable from downstream.
        pub_export_macro: true,
    });
}
