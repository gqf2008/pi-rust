//! Guest SDK for writing **kiqi wasm extensions**.
//!
//! Wraps [`wit_bindgen::generate!`] over `extensions/kiqi-extension.wit` (ABI 0.8.0)
//! so an extension crate only has to implement the generated `Guest` trait and call
//! [`bindings::export!`]:
//!
//! ```ignore
//! use kiqi_wasm_bindings::bindings::exports::kiqi::extension::extension_api::Guest;
//!
//! struct MyExtension;
//! impl Guest for MyExtension { /* ... */ }
//! kiqi_wasm_bindings::bindings::export!(MyExtension with_types_in kiqi_wasm_bindings::bindings);
//! ```
//!
//! The generated bindings live in [`bindings`] so `export!` can be invoked from
//! the extension crate (`with_types_in kiqi_wasm_bindings::bindings`).

pub mod bindings {
    wit_bindgen::generate!({
        path: "../../kiqi-extension.wit",
        world: "extension",
        // The extension crate, not this SDK crate, implements the exported trait
        // and calls `export!`, so both the macro and the generated items must be
        // reachable from downstream.
        pub_export_macro: true,
    });
}

/// Implement the `oauth-api` and `provider-api` contribution exports as
/// empty/denied stubs, leaving `render-api` to the crate.
///
/// Useful for a guest that actually contributes a markdown renderer (the
/// conformance fixture does): the component model still requires the other two
/// faces, but only the render trait may be implemented by hand.
#[macro_export]
macro_rules! deny_oauth_and_provider_exports {
    ($extension:ty) => {
        impl $crate::bindings::exports::kiqi::extension::oauth_api::Guest for $extension {
            fn list_providers(
            ) -> ::std::vec::Vec<$crate::bindings::kiqi::extension::types::OauthProvider> {
                ::std::vec::Vec::new()
            }

            fn action(
                _provider_id: ::std::string::String,
                _action: $crate::bindings::kiqi::extension::types::OauthAction,
            ) -> ::core::result::Result<
                $crate::bindings::kiqi::extension::types::OauthResponse,
                $crate::bindings::kiqi::extension::types::ExtensionError,
            > {
                ::core::result::Result::Err(
                    $crate::bindings::kiqi::extension::types::ExtensionError::Denied(
                        "oauth is not granted".to_string(),
                    ),
                )
            }
        }

        impl $crate::bindings::exports::kiqi::extension::provider_api::Guest for $extension {
            fn list_providers(
            ) -> ::std::vec::Vec<$crate::bindings::kiqi::extension::types::ProviderSpec> {
                ::std::vec::Vec::new()
            }

            fn request(
                _request: ::std::string::String,
            ) -> ::core::result::Result<
                ::std::string::String,
                $crate::bindings::kiqi::extension::types::ExtensionError,
            > {
                ::core::result::Result::Err(
                    $crate::bindings::kiqi::extension::types::ExtensionError::Denied(
                        "provider requests are not granted".to_string(),
                    ),
                )
            }
        }
    };
}

/// Implement all three 0.8.0 contribution exports — OAuth, provider and render
/// — as empty/denied stubs.
///
/// The component model makes every export in `world extension` mandatory, so even
/// an extension that contributes none of these faces has to implement their
/// traits. Denying here matches the host: a face the extension never declared is
/// never called, and calling it anyway fails closed.
#[macro_export]
macro_rules! deny_contribution_exports {
    ($extension:ty) => {
        $crate::deny_oauth_and_provider_exports!($extension);

        impl $crate::bindings::exports::kiqi::extension::render_api::Guest for $extension {
            fn transform_markdown(
                _markdown: ::std::string::String,
            ) -> ::core::result::Result<
                ::std::string::String,
                $crate::bindings::kiqi::extension::types::ExtensionError,
            > {
                ::core::result::Result::Err(
                    $crate::bindings::kiqi::extension::types::ExtensionError::Denied(
                        "render is not granted".to_string(),
                    ),
                )
            }
        }
    };
}
