//! The session's extension-state snapshot.
//!
//! Historically this module accumulated native (cdylib) registrations during an
//! extension's `kiqi_extension_register` call and snapshotted them for the host
//! session. The cdylib channel was retired on 2026-10-26 and the C-ABI
//! registration machine that fed it was removed in the pure-serde event-bus
//! batch (2026-10-27, `docs/design/extension-abi.md` §6.49/§6.50). What
//! survives is the snapshot itself: the **per-session sink** (what actually
//! routes a session's dispatch), the **per-session capability bundle**
//! (`SessionCaps`, what probes/commands/prompt transforms call with), plus the
//! historical `active` staleness flag.
//!
//! The staleness flag is **no longer consulted by any dispatch path** — the
//! native-handler funnels that checked it are gone, and the sink belongs to the
//! snapshot, so a swapped-out session cannot dispatch with its old caps anyway.
//! `invalidate`/`assert_active` remain for API compatibility and for callers
//! that want to mark a session closed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::compiled::CompiledEventSink;
use crate::SessionCaps;

/// An immutable snapshot of a host session's extension state.
///
/// It carries the session's `active` flag plus — when the session has one — the
/// session's compiled/wasm event sink and capability bundle. All are cheap
/// `Arc`s, so cloning a snapshot shares the same flag, sink and caps; a
/// `/reload` builds a fresh snapshot (with a fresh sink and caps) and
/// invalidates the old one.
#[derive(Clone)]
pub struct RegistrySnapshot {
    active: Arc<AtomicBool>,
    /// The session's own event sink. `Some` makes every dispatch from this
    /// snapshot go here instead of the process-global fallback, which is what
    /// keeps concurrent sessions (ACP) from cross-talking. `None` is the
    /// compatible path: dispatch falls back to the process-global sink.
    compiled_sink: Option<CompiledEventSink>,
    /// The session's own capability bundle. Probes, extension commands and
    /// prompt transforms read this so a guest in session A can only act on
    /// session A (cancel, inject, set_status), never on whichever session was
    /// built last. `None` is the sessionless path (tests / a host that has no
    /// owned capability bundle): callers fall back to the process-global slot.
    session_caps: Option<Arc<dyn SessionCaps>>,
}

impl RegistrySnapshot {
    /// A fresh, active snapshot. Never carries native registrations — there is
    /// no native registration channel any more.
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            compiled_sink: None,
            session_caps: None,
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

    /// Attach the session's capability bundle (builder form).
    ///
    /// The bundle stays behind the snapshot so a `/reload` that swaps the
    /// snapshot also swaps capabilities atomically with the sink.
    pub fn with_session_caps(mut self, caps: Arc<dyn SessionCaps>) -> Self {
        self.session_caps = Some(caps);
        self
    }

    /// The session's own capability bundle, if one was attached. Call sites
    /// without a snapshot (or with a snapshot built before caps were tracked)
    /// fall back to the process-global slot.
    pub fn session_caps(&self) -> Option<&Arc<dyn SessionCaps>> {
        self.session_caps.as_ref()
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

    struct SilentLog;
    impl crate::Log for SilentLog {
        fn log(&self, _level: crate::LogLevel, _message: &str) {}
    }

    struct MarkerCaps;
    impl SessionCaps for MarkerCaps {
        fn host_caps(&self) -> crate::HostCaps<'_> {
            static LOG: SilentLog = SilentLog;
            crate::HostCaps::observer(&LOG)
        }
    }

    #[test]
    fn cloning_a_snapshot_shares_the_session_caps() {
        let caps: Arc<dyn SessionCaps> = Arc::new(MarkerCaps);
        let snapshot = RegistrySnapshot::new().with_session_caps(Arc::clone(&caps));
        let clone = snapshot.clone();

        let original = snapshot.session_caps().expect("caps are attached");
        let copied = clone.session_caps().expect("the clone carries them");
        assert!(Arc::ptr_eq(original, copied));
        assert!(copied.host_caps().session.is_none());
    }
}
