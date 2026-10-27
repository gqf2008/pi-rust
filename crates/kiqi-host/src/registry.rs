//! The session's extension-state snapshot.
//!
//! Historically this module accumulated native (cdylib) registrations during an
//! extension's `kiqi_extension_register` call and snapshotted them for the host
//! session. The cdylib channel was retired on 2026-10-26 and the C-ABI
//! registration machine that fed it was removed in the pure-serde event-bus
//! batch (2026-10-27, `docs/design/extension-abi.md` §6.49/§6.50). What
//! survives is the snapshot itself: the **per-session sink** (what actually
//! routes a session's dispatch) plus the historical `active` staleness flag.
//!
//! The staleness flag is **no longer consulted by any dispatch path** — the
//! native-handler funnels that checked it are gone, and the sink belongs to the
//! snapshot, so a swapped-out session cannot dispatch with its old caps anyway.
//! `invalidate`/`assert_active` remain for API compatibility and for callers
//! that want to mark a session closed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::compiled::CompiledEventSink;

/// An immutable snapshot of a host session's extension state.
///
/// It carries the session's `active` flag plus — when the session has one — the
/// session's compiled/wasm event sink. Both are cheap `Arc`s, so cloning a
/// snapshot shares the same flag and the same sink; a `/reload` builds a fresh
/// snapshot (and sink) and invalidates the old one.
pub struct RegistrySnapshot {
    active: Arc<AtomicBool>,
    /// The session's own event sink. `Some` makes every dispatch from this
    /// snapshot go here instead of the process-global fallback, which is what
    /// keeps concurrent sessions (ACP) from cross-talking. `None` is the
    /// compatible path: dispatch falls back to the process-global sink.
    compiled_sink: Option<CompiledEventSink>,
}

impl Clone for RegistrySnapshot {
    /// Shares the active flag and the sink `Arc` — cloning a snapshot never
    /// clones the sink closure or detaches the staleness flag.
    fn clone(&self) -> Self {
        Self {
            active: Arc::clone(&self.active),
            compiled_sink: self.compiled_sink.clone(),
        }
    }
}

impl RegistrySnapshot {
    /// A fresh, active snapshot. Never carries native registrations — there is
    /// no native registration channel any more.
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            compiled_sink: None,
        }
    }

    /// Attach the session's compiled/wasm event sink (builder form).
    pub fn with_compiled_sink(mut self, sink: CompiledEventSink) -> Self {
        self.compiled_sink = Some(sink);
        self
    }

    /// The session's own event sink, if one was attached. Dispatch funnels pass
    /// this to [`crate::compiled`]'s `*_with` variants, which fall back to the
    /// process-global sink when it is `None`.
    pub fn compiled_sink(&self) -> Option<&CompiledEventSink> {
        self.compiled_sink.as_ref()
    }

    /// Whether this snapshot's session is still active.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// The shared active flag (for [`assert_active`] and `/reload` swaps).
    pub fn active_flag(&self) -> &Arc<AtomicBool> {
        &self.active
    }

    /// Mark this snapshot's session stale (a `/reload` swaps it out).
    pub fn invalidate(&self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

impl Default for RegistrySnapshot {
    fn default() -> Self {
        Self::new()
    }
}

/// Staleness guard: `true` while the owning session is still active. Called
/// before dispatching an extension event so a stale snapshot (from a
/// swapped-out session) can't act.
pub fn assert_active(active: &Arc<AtomicBool>) -> bool {
    active.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_snapshot_is_active_and_invalidate_flips_it() {
        let snapshot = RegistrySnapshot::new();
        assert!(snapshot.is_active());
        assert!(assert_active(snapshot.active_flag()));
        snapshot.invalidate();
        assert!(!snapshot.is_active());
        assert!(!assert_active(snapshot.active_flag()));
    }

    #[test]
    fn cloning_a_snapshot_shares_the_flag_and_the_sink() {
        use crate::compiled::CompiledVerdict;
        use crate::EventTag;
        use std::sync::atomic::AtomicUsize;

        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let snapshot = RegistrySnapshot::new().with_compiled_sink(Arc::new(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            CompiledVerdict::Continue
        }));
        let clone = snapshot.clone();

        // The sink Arc is shared: dispatching through the clone reaches it.
        assert!(clone.compiled_sink().is_some());
        let event = crate::Event::new(EventTag::SessionStart, serde_json::Value::Null);
        assert_eq!(
            crate::compiled::dispatch_observe_with(clone.compiled_sink(), &event),
            CompiledVerdict::Continue
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            crate::compiled::dispatch_observe_with(clone.compiled_sink(), &event),
            CompiledVerdict::Continue
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        // The active flag is shared too.
        clone.invalidate();
        assert!(!snapshot.is_active());
    }
}
