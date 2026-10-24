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

/// Implement the 0.8.0 contribution exports — OAuth, provider and render — as
/// empty/denied stubs.
///
/// The component model makes every export in `world plugin` mandatory, so even
/// a plugin that contributes none of these faces has to implement their
/// traits. Denying here matches the host: a face the plugin never declared is
/// never called, and calling it anyway fails closed.
#[macro_export]
macro_rules! deny_contribution_exports {
    ($plugin:ty) => {
        impl $crate::bindings::exports::kiqi::plugin::oauth_api::Guest for $plugin {
            fn list_providers(
            ) -> ::std::vec::Vec<$crate::bindings::kiqi::plugin::types::OauthProvider> {
                ::std::vec::Vec::new()
            }

            fn action(
                _provider_id: ::std::string::String,
                _action: $crate::bindings::kiqi::plugin::types::OauthAction,
            ) -> ::core::result::Result<
                $crate::bindings::kiqi::plugin::types::OauthResponse,
                $crate::bindings::kiqi::plugin::types::PluginError,
            > {
                ::core::result::Result::Err(
                    $crate::bindings::kiqi::plugin::types::PluginError::Denied(
                        "oauth is not granted".to_string(),
                    ),
                )
            }
        }

        impl $crate::bindings::exports::kiqi::plugin::provider_api::Guest for $plugin {
            fn list_providers(
            ) -> ::std::vec::Vec<$crate::bindings::kiqi::plugin::types::ProviderSpec> {
                ::std::vec::Vec::new()
            }

            fn request(
                _request: ::std::string::String,
            ) -> ::core::result::Result<
                ::std::string::String,
                $crate::bindings::kiqi::plugin::types::PluginError,
            > {
                ::core::result::Result::Err(
                    $crate::bindings::kiqi::plugin::types::PluginError::Denied(
                        "provider requests are not granted".to_string(),
                    ),
                )
            }
        }

        impl $crate::bindings::exports::kiqi::plugin::render_api::Guest for $plugin {
            fn transform_markdown(
                _markdown: ::std::string::String,
            ) -> ::core::result::Result<
                ::std::string::String,
                $crate::bindings::kiqi::plugin::types::PluginError,
            > {
                ::core::result::Result::Err(
                    $crate::bindings::kiqi::plugin::types::PluginError::Denied(
                        "render is not granted".to_string(),
                    ),
                )
            }
        }
    };
}
