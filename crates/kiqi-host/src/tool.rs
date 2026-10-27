//! [`ToolCallContext`] — the per-session context of an extension tool call.
//!
//! [`crate::HOST_CONTEXT_KEY`] is a **legacy** constant from the retired
//! C-ABI channel: nothing injects it any more. The wasm contract carries
//! session identity through `HostCaps` (session-read) instead.
//!
//! This module used to also carry `ExtensionToolHandle` / `ExtensionToolAdapter`,
//! the C-ABI tool lifecycle. The cdylib channel was retired on 2026-10-26 and
//! the registration machine was removed in the pure-serde event-bus batch
//! (`docs/design/extension-abi.md` §6.49); wasm tool calls go through the
//! `Extension::call_tool` contract instead.

use std::sync::{Arc, RwLock};

use serde_json::Value;

/// Per-session context an extension tool call carries (session id + cwd).
///
/// Kept as a plain Rust type for host callers; the wasm contract carries the
/// same facts through `HostCaps`, not through a `__ki` argument injection.
///
/// A built-in tool gets an `ExecutionToolContext` from the harness and can
/// resolve the working directory from it; an extension tool has no such channel.
/// Without this, an extension that needs project (or session) identity has to ask
/// the model for it — and the model does not know the session id, while any
/// `cwd` it guesses is a silent way to split one store into two.
///
/// It is filled in two steps because the CLI assembles the tool set *before* the
/// session it belongs to exists: [`ToolCallContext::new`] carries the `cwd`, and
/// [`ToolCallContext::set_session_id`] completes it once the session does.
/// Every adapter for one session shares one instance (`Arc` inside), so the late
/// write is visible to all of them — including across an extension reload, which
/// rebuilds the adapters from the same context.
///
/// Unknown fields are absent, not empty strings: a consumer should prefer a
/// project-scoped fallback when [`ToolCallContext::session_id`] is `None`.
#[derive(Clone, Debug, Default)]
pub struct ToolCallContext {
    inner: Arc<RwLock<ToolCallContextState>>,
}

#[derive(Debug, Default)]
struct ToolCallContextState {
    cwd: Option<String>,
    session_id: Option<String>,
}

impl ToolCallContext {
    /// A context that knows the project directory but not yet the session id.
    /// An empty `cwd` is treated as unknown.
    pub fn new(cwd: impl Into<String>) -> Self {
        let cwd = cwd.into();
        Self {
            inner: Arc::new(RwLock::new(ToolCallContextState {
                cwd: (!cwd.is_empty()).then_some(cwd),
                session_id: None,
            })),
        }
    }

    /// Record the session id once the session exists. Safe to call again (a
    /// reload re-derives it); a poisoned lock is ignored rather than panicking
    /// inside a tool call.
    pub fn set_session_id(&self, session_id: impl Into<String>) {
        if let Ok(mut state) = self.inner.write() {
            state.session_id = Some(session_id.into());
        }
    }

    /// The project directory the session was launched in, when known.
    pub fn cwd(&self) -> Option<String> {
        self.inner.read().ok().and_then(|s| s.cwd.clone())
    }

    /// The session id, when the session has one and the host has recorded it.
    pub fn session_id(&self) -> Option<String> {
        self.inner.read().ok().and_then(|s| s.session_id.clone())
    }

    /// The object to inject, or `None` when nothing is known yet — in which
    /// case arguments are passed through untouched rather than gaining an empty
    /// `{}` that an extension would have to special-case.
    pub fn to_json(&self) -> Option<Value> {
        let state = self.inner.read().ok()?;
        if state.cwd.is_none() && state.session_id.is_none() {
            return None;
        }
        let mut object = serde_json::Map::new();
        if let Some(cwd) = &state.cwd {
            object.insert("cwd".to_string(), Value::String(cwd.clone()));
        }
        if let Some(session_id) = &state.session_id {
            object.insert("sessionId".to_string(), Value::String(session_id.clone()));
        }
        Some(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_context_carries_cwd_and_session_only_when_known() {
        // Nothing known yet → no key at all, rather than an empty object a
        // extension would have to special-case.
        assert!(ToolCallContext::default().to_json().is_none());
        assert!(ToolCallContext::new("").to_json().is_none());

        let context = ToolCallContext::new("D:\\proj");
        let json = context.to_json().expect("cwd alone is enough to inject");
        assert_eq!(json["cwd"], "D:\\proj");
        assert!(json.get("sessionId").is_none(), "not yet recorded");

        context.set_session_id("sess-1");
        let json = context.to_json().unwrap();
        assert_eq!(json["cwd"], "D:\\proj");
        assert_eq!(json["sessionId"], "sess-1");
    }
}
