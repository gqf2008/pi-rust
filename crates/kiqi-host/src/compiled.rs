//! Extension event fan-out.
//!
//! Extensions observe events through [`crate::Extension::on_event`]; this module
//! is the seam. The dispatch funnels in [`crate::translate`] call
//! [`dispatch_observe`] / [`dispatch_claiming`] / [`dispatch_provider_gate`], and
//! the host installs a [`CompiledEventSink`] whose closure drives every loaded
//! wasm host. No sink installed (a bare `kiqi-host` build, or tests) makes every
//! call a no-op.
//!
//! # Per-session sinks
//!
//! A sink may belong to one session rather than the process: the session's
//! [`RegistrySnapshot`](crate::RegistrySnapshot) carries it, and the `*_with`
//! variants below dispatch through that sink. The plain variants pass `None`,
//! which falls back to the process-global slot installed by
//! [`set_compiled_event_sink`] — the compatibility path for call sites that have
//! no snapshot. A snapshot that carries its own sink never consults the global
//! slot, so two sessions in one process (the ACP multi-session server) cannot
//! cross-talk.
//!
//! Events cross the seam as the pure-serde [`crate::Event`] — the same type the
//! extension contract hands a backend — so a sink needs no translation and no
//! `repr(C)` wire format.

use std::sync::{Arc, OnceLock, RwLock};

use crate::Event;
use serde_json::Value;

/// What the extensions decided about one event.
///
/// [`Self::Claimed`] maps to `crate::EventAction::Claimed` (the extension took the
/// event). [`Self::Patch`] is the one mutation channel: it exists only for the
/// provider gate, which asks patch-capable hosts on their own dispatch and
/// applies the validated result to the live request options. An observe or
/// claiming fan-out never consumes it.
#[derive(Debug, Clone, PartialEq)]
pub enum CompiledVerdict {
    /// No extension objected; later handlers still run.
    Continue,
    /// An extension consumed the event: a routing dispatcher must stop and report it
    /// handled.
    Claimed,
    /// A provider-gate patch (already validated by the host that produced it).
    Patch(Value),
}

/// Whether a dispatch may wait for a busy extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Advisory fan-out (`dispatch_to_handlers`, lifecycle): an extension that is
    /// mid-call is skipped rather than waited for.
    Observe,
    /// Routing fan-out (claiming dispatchers): the verdict decides control
    /// flow, so the call waits — failing open would drop a veto.
    Claiming,
    /// Provider gate: only patch-capable hosts are invoked, and only they may
    /// return [`CompiledVerdict::Patch`].
    ProviderGate,
}

/// Receives one dispatched event and reports what the extensions decided.
pub type CompiledEventSink = Arc<dyn Fn(DispatchMode, &Event) -> CompiledVerdict + Send + Sync>;

/// Process-global fallback sink. Per-session sinks live on the session's
/// [`RegistrySnapshot`](crate::RegistrySnapshot); this slot answers the call
/// sites that have no snapshot (and the tests that drive the funnels directly).
static SINK: OnceLock<RwLock<Option<CompiledEventSink>>> = OnceLock::new();

fn sink_slot() -> &'static RwLock<Option<CompiledEventSink>> {
    SINK.get_or_init(|| RwLock::new(None))
}

/// Install (or replace) the process-global fallback sink.
///
/// Idempotent; last writer wins across session builds. This is only the
/// fallback for call sites without a session snapshot: a session that carries
/// its own sink on [`RegistrySnapshot`](crate::RegistrySnapshot) dispatches
/// through that one instead, so concurrent sessions stay isolated.
pub fn set_compiled_event_sink(sink: CompiledEventSink) {
    *sink_slot().write().unwrap() = Some(sink);
}

/// Whether a sink is installed (a session build has reached this process).
///
/// The provider-hook bridge uses this to install itself for a wasm-only
/// install: the compiled/wasm fan-out is a live observer even with no native
/// handlers.
pub fn sink_installed() -> bool {
    sink_slot().read().unwrap().is_some()
}

/// Remove the sink, returning the host to no compiled fan-out.
///
/// The running host never tears it down; this exists so a test can restore the
/// process-global state it shares with the rest of the test binary.
#[doc(hidden)]
pub fn clear_compiled_event_sink() {
    *sink_slot().write().unwrap() = None;
}

/// Fan one event out to the compiled/wasm extensions, if a sink is installed.
/// Observe fan-out: a busy extension is skipped, never waited for.
///
/// Compatibility entry point (no session snapshot): the `_with` form is what the
/// dispatch funnels use now, and they pass `None` for this same global path.
#[allow(dead_code)] // kept as the named no-snapshot entry point; funnels call `_with`
pub(crate) fn dispatch_observe(event: &Event) -> CompiledVerdict {
    dispatch_observe_with(None, event)
}

/// Observe fan-out through `sink` when the session has one, else the global
/// fallback. `None` means "this call site has no session sink".
pub(crate) fn dispatch_observe_with(
    sink: Option<&CompiledEventSink>,
    event: &Event,
) -> CompiledVerdict {
    dispatch_with(sink, DispatchMode::Observe, event)
}

/// Routing variant: the caller acts on the verdict, so a busy extension is waited
/// for rather than skipped.
///
/// Compatibility entry point; see [`dispatch_observe`].
#[allow(dead_code)] // kept as the named no-snapshot entry point; funnels call `_with`
pub(crate) fn dispatch_claiming(event: &Event) -> CompiledVerdict {
    dispatch_claiming_with(None, event)
}

/// Claiming fan-out through `sink` when the session has one, else the global
/// fallback.
pub(crate) fn dispatch_claiming_with(
    sink: Option<&CompiledEventSink>,
    event: &Event,
) -> CompiledVerdict {
    dispatch_with(sink, DispatchMode::Claiming, event)
}

/// Provider-gate variant: asks only the patch-capable hosts and returns the
/// merged patch, if any. The caller applies it; an observe fan-out never does.
///
/// Compatibility entry point; see [`dispatch_observe`].
#[allow(dead_code)] // kept as the named no-snapshot entry point; funnels call `_with`
pub(crate) fn dispatch_provider_gate(event: &Event) -> Option<Value> {
    dispatch_provider_gate_with(None, event)
}

/// Provider-gate variant through `sink` when the session has one, else the
/// global fallback.
pub(crate) fn dispatch_provider_gate_with(
    sink: Option<&CompiledEventSink>,
    event: &Event,
) -> Option<Value> {
    match dispatch_with(sink, DispatchMode::ProviderGate, event) {
        CompiledVerdict::Patch(value) => Some(value),
        _ => None,
    }
}

fn dispatch_with(
    sink: Option<&CompiledEventSink>,
    mode: DispatchMode,
    event: &Event,
) -> CompiledVerdict {
    // A session sink is authoritative: it must never fall through to another
    // session's global install.
    if let Some(sink) = sink {
        return sink(mode, event);
    }
    let slot = sink_slot().read().unwrap();
    let Some(sink) = slot.as_ref() else {
        return CompiledVerdict::Continue;
    };
    sink(mode, event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventTag;
    use std::sync::Mutex;

    /// Serializes the sink tests across the crate; the sink is process-global.
    use crate::SINK_TEST_LOCK as TEST_LOCK;

    #[test]
    fn a_sink_observes_data_and_empty_events() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let seen: Arc<Mutex<Vec<(DispatchMode, EventTag, Value)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        set_compiled_event_sink(Arc::new(move |mode, event| {
            record
                .lock()
                .unwrap()
                .push((mode, event.tag, event.payload.clone()));
            CompiledVerdict::Continue
        }));

        assert!(!crate::translate::dispatch_data_event(
            EventTag::BeforeAgentStart,
            r#"{"prompt":"hi"}"#,
        ));
        assert!(!crate::translate::dispatch_empty_event(
            EventTag::SessionStart,
        ));

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|(mode, tag, payload)| *mode == DispatchMode::Observe
                    && *tag == EventTag::BeforeAgentStart
                    && payload["prompt"] == "hi"),
            "the data payload must arrive parsed and marked observe: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|(mode, tag, payload)| *mode == DispatchMode::Observe
                    && *tag == EventTag::SessionStart
                    && payload.is_null()),
            "the empty lifecycle payload must arrive null: {seen:?}"
        );
        drop(seen);
        clear_compiled_event_sink();
    }

    /// A wasm-only install must still fan events into the compiled/wasm sink.
    #[test]
    fn an_extension_free_session_still_dispatches() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        set_compiled_event_sink(Arc::new(move |_mode, _event| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            CompiledVerdict::Continue
        }));

        assert!(
            !crate::translate::dispatch_empty_event(EventTag::SessionStart),
            "no native handler subscribes"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a ki with no native handlers must still fan events into the compiled/wasm sink"
        );

        clear_compiled_event_sink();
    }

    #[test]
    fn without_a_sink_dispatch_is_a_noop() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_compiled_event_sink();
        let event = Event::new(EventTag::SessionStart, Value::Null);
        assert_eq!(dispatch_observe(&event), CompiledVerdict::Continue);
    }

    /// A claim from the compiled side reaches the routing dispatcher, so a
    /// compiled extension can consume a key.
    #[test]
    fn a_claimed_verdict_reaches_the_routing_dispatcher() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_compiled_event_sink(Arc::new(|mode, _| {
            assert_eq!(
                mode,
                DispatchMode::Claiming,
                "routing dispatch must ask the blocking face"
            );
            CompiledVerdict::Claimed
        }));
        assert!(crate::translate::dispatch_data_event_claiming(
            EventTag::Input,
            r#"{"key":"f8"}"#,
        ));
        clear_compiled_event_sink();

        // With no sink the same call is unclaimed.
        assert!(!crate::translate::dispatch_data_event_claiming(
            EventTag::Input,
            r#"{"key":"f8"}"#,
        ));
    }
}
