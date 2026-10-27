//! The host session's extension-lifetime holder.
//!
//! P6 retired the cdylib channel (the `libloading` loader, the
//! `kiqi_extension_register` entrypoint contract, `ki install` / `ki dev`), and
//! the pure-serde event-bus batch removed the C-ABI registration machine that
//! fed it (`docs/design/extension-abi.md` §6.49). Extensions ship as wasm
//! guests and drive the host through `kiqi-host`'s wasm backend.
//!
//! What survives here is [`ExtensionSession`] — the per-harness holder of the
//! session snapshot the event funnels dispatch through, plus the session's
//! [`ActionBridge`]. A session always presents a **dispatchable** (empty)
//! snapshot, which is exactly what a wasm-only install must do: the funnels
//! reach the compiled/wasm sink unconditionally (see [`crate::compiled`]).

use std::sync::Arc;

use crate::compiled::CompiledEventSink;
use crate::registry::RegistrySnapshot;
use crate::{ActionBridge, SessionCaps};

/// The host session's extension state: a snapshot of the session's extension
/// state plus the session's action bridge.
///
/// `Clone` (B5d): every field is cheaply clonable (`Option<Arc<..>>`), so the
/// reload routine can clone the live session out of its `Mutex` cell for local
/// inspection and store a fresh one back in — without a borrow spanning the
/// store.
#[derive(Clone)]
pub struct ExtensionSession {
    snapshot: Option<Arc<RegistrySnapshot>>,
    /// B5a: the session's action bridge (`None` when no extensions / tests / the
    /// `--no-extensions` path). Kept here so `kiqi-cli` can recover it after
    /// `AgentHarness::create` to call `set_harness` — the bridge must outlive
    /// any extension→host `runtime_action` call. Cloning is cheap (an `Arc`).
    action_bridge: Option<Arc<ActionBridge>>,
}

impl ExtensionSession {
    /// An empty session (no extensions — `--no-extensions`, no wasm guests, or
    /// tests).
    ///
    /// Carries an **empty snapshot**, not `None`: the event fan-out hangs off
    /// the snapshot the host threads into `translate`'s funnels — which reach
    /// the compiled/wasm sink unconditionally, precisely so a `ki` with no
    /// extensions still delivers events. A `None` snapshot therefore used to gate
    /// every event call site off (a wasm-only install delivered nothing, not
    /// even to `on_event`).
    pub fn none() -> Self {
        Self::none_with_compiled_sink(None)
    }

    /// An empty session that carries `sink` as its **per-session** event sink.
    ///
    /// `kiqi-cli` installs the compiled/wasm sink on the session before it
    /// builds the emitter / provider hooks, so everything this session
    /// dispatches goes through its own sink instead of the process-global
    /// fallback. Two sessions in one process (the ACP multi-session server)
    /// therefore cannot cross-talk. `None` leaves the snapshot on the global
    /// fallback path.
    pub fn none_with_compiled_sink(sink: Option<CompiledEventSink>) -> Self {
        Self::none_with_compiled_sink_and_caps(sink, None)
    }

    /// Like [`none_with_compiled_sink`](Self::none_with_compiled_sink), but the
    /// snapshot also carries the session's capability bundle.
    ///
    /// This is the production constructor: probes, extension commands and
    /// prompt transforms resolve capabilities from the snapshot, so two
    /// sessions in one process (the ACP multi-session server) each use their
    /// own caps instead of the last-built session's. `None` keeps the
    /// sessionless fallback to the process-global slot.
    pub fn none_with_compiled_sink_and_caps(
        sink: Option<CompiledEventSink>,
        caps: Option<Arc<dyn SessionCaps>>,
    ) -> Self {
        let mut snapshot = RegistrySnapshot::new();
        if let Some(sink) = sink {
            snapshot = snapshot.with_compiled_sink(sink);
        }
        if let Some(caps) = caps {
            snapshot = snapshot.with_session_caps(caps);
        }
        Self::from_parts(Arc::new(snapshot), None)
    }

    /// Assemble a session from already-loaded parts (tests, or a host that
    /// assembled a snapshot by other means).
    pub fn from_parts(
        snapshot: Arc<RegistrySnapshot>,
        action_bridge: Option<Arc<ActionBridge>>,
    ) -> Self {
        Self {
            snapshot: Some(snapshot),
            action_bridge,
        }
    }

    /// The session snapshot. Borrowed view; for an owned share (e.g. handing to
    /// the emitter) use [`snapshot_arc`](Self::snapshot_arc).
    ///
    /// Always `Some` for sessions this module builds, including an extension-free
    /// [`none`](Self::none).
    pub fn snapshot(&self) -> Option<&RegistrySnapshot> {
        self.snapshot.as_deref()
    }

    /// A shared (`Arc`) clone of the session snapshot, for host code that must
    /// keep it alive beyond this session local — notably the
    /// [`ExtensionEmitter`](crate::ExtensionEmitter) installed into
    /// `AgentHarnessOptions.agent_emitter`.
    ///
    /// Always `Some` for sessions this module builds, including an extension-free
    /// [`none`](Self::none). The event call sites match on this value to decide
    /// whether to enter `translate`'s funnels at all, and those funnels reach the
    /// compiled/wasm sink unconditionally.
    pub fn snapshot_arc(&self) -> Option<Arc<RegistrySnapshot>> {
        self.snapshot.clone()
    }

    /// The session's capability bundle, when its snapshot carries one.
    /// Callers outside a tool invocation use this instead of the process-wide
    /// slot so a guest can only act on the session it belongs to.
    pub fn session_caps(&self) -> Option<&Arc<dyn SessionCaps>> {
        self.snapshot.as_ref().and_then(|snap| snap.session_caps())
    }

    /// Whether any extension claimed ownership of this host process's stdio
    /// (`declare {"ownsStdio":true}`). Always `false`: the wasm contract has no
    /// stdio claim and the native declaration channel is gone.
    pub fn owns_stdio(&self) -> bool {
        false
    }

    /// Whether the session carries no native registrations. Always `true` — the
    /// native registration channel is gone; extensions live in the wasm backend.
    pub fn is_empty(&self) -> bool {
        true
    }

    /// B5a: the session's action bridge, if one was threaded in. `kiqi-cli`
    /// recovers this after `AgentHarness::create` succeeds to call
    /// `HarnessActionHost::set_harness`. `None` when no extensions loaded /
    /// tests / `--no-extensions`.
    pub fn action_bridge(&self) -> Option<Arc<ActionBridge>> {
        self.action_bridge.clone()
    }
}
