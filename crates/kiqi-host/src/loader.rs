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

use crate::registry::RegistrySnapshot;
use crate::ActionBridge;

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
        Self {
            snapshot: Some(Arc::new(RegistrySnapshot::new())),
            action_bridge: None,
        }
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
