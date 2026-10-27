//! `resources_discover` compatibility shim.
//!
//! Historically this module fanned a `resources_discover` event out to native
//! (cdylib) handlers that returned skill/prompt/theme paths. The native
//! registration channel was removed in the pure-serde event-bus batch
//! (`docs/design/extension-abi.md` §6.49); wasm packages contribute resources
//! through their verified `resources.json` + `resources/` tree
//! ([`crate::wasm`] and `kiqi-cli`'s package-resource loader), so there is
//! nothing left to fan out.
//!
//! The type and function survive so the CLI's resource-loading composition
//! keeps one shape for "extension-contributed paths"; the result is always
//! empty now.

use crate::registry::RegistrySnapshot;

/// The merged `resources_discover` result: bare string arrays for skills,
/// prompt-templates, and themes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiscoveredResources {
    pub skill_paths: Vec<String>,
    pub prompt_paths: Vec<String>,
    pub theme_paths: Vec<String>,
}

/// No-op compatibility entry point. Always returns an empty
/// [`DiscoveredResources`]; wasm resources arrive through the package tree.
pub fn emit_resources_discover(
    _cwd: &str,
    _reason: &str,
    _snapshot: &RegistrySnapshot,
) -> DiscoveredResources {
    DiscoveredResources::default()
}
