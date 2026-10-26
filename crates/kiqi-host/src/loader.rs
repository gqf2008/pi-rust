//! The host session's extension-lifetime holder.
//!
//! P6: the cdylib channel is retired (the `libloading` loader, the
//! `kiqi_extension_register` entrypoint contract, `ki install` / `ki dev`).
//! Extensions ship as wasm guests and drive the host through `kiqi-extension-host`.
//! What survives here is [`ExtensionSession`] — the per-harness holder of the
//! registry snapshot the event funnels dispatch through, plus the session's
//! [`ActionBridge`]. Without a loader it presents an **empty registry**, which
//! is exactly what a wasm-only install must do: the funnels reach the
//! compiled/wasm sink unconditionally (see [`crate::compiled`]).

use std::sync::Arc;

use crate::registry::{ExtensionRegistry, RegistrySnapshot};
use crate::ActionBridge;

/// The host session's extension state: a snapshot of the merged registry
/// (tools/commands/handlers) plus the session's action bridge.
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
    /// Carries an **empty registry**, not `None`: the event fan-out hangs off
    /// the snapshot the host threads into `translate`'s funnels — which call
    /// the compiled/wasm sink unconditionally, precisely so a `ki` with no
    /// extensions still delivers events. A `None` snapshot therefore used to gate
    /// every event call site off (a wasm-only install delivered nothing, not
    /// even to `on_event`).
    pub fn none() -> Self {
        Self {
            snapshot: Some(Arc::new(ExtensionRegistry::new().snapshot())),
            action_bridge: None,
        }
    }

    /// Assemble a session from already-loaded parts (tests, or a host that
    /// assembled a registry by other means).
    pub fn from_parts(
        snapshot: Arc<RegistrySnapshot>,
        action_bridge: Option<Arc<ActionBridge>>,
    ) -> Self {
        Self {
            snapshot: Some(snapshot),
            action_bridge,
        }
    }

    /// The merged registry snapshot (tools/commands/handlers) the registry tier
    /// registered. Borrowed view for iterating tools/commands; for an owned
    /// share (e.g. handing to the emitter) use [`snapshot_arc`](Self::snapshot_arc).
    ///
    /// Always `Some` for sessions this module builds, including an extension-free
    /// [`none`](Self::none) (an empty registry); [`snapshot_arc`](Self::snapshot_arc)
    /// carries the reason the `Option` is not collapsed away.
    pub fn snapshot(&self) -> Option<&RegistrySnapshot> {
        self.snapshot.as_deref()
    }

    /// A shared (`Arc`) clone of the merged registry snapshot, for host code that
    /// must keep the snapshot alive beyond this session local — notably the
    /// [`ExtensionEmitter`](crate::ExtensionEmitter) installed into
    /// `AgentHarnessOptions.agent_emitter`.
    ///
    /// Always `Some` for sessions this module builds, including an extension-free
    /// [`none`](Self::none) (an empty registry). The event call sites match on
    /// this value to decide whether to enter `translate`'s funnels at all, and
    /// those funnels reach the compiled/wasm sink unconditionally — so treating
    /// "no extensions" as `None` used to switch the whole extension event surface off.
    pub fn snapshot_arc(&self) -> Option<Arc<RegistrySnapshot>> {
        self.snapshot.clone()
    }

    /// Whether any registered extension claimed ownership of this host process's
    /// stdio (`declare {"ownsStdio":true}`). When `true`, the host (pi-cli)
    /// must run the headless `Rpc` lifecycle (dispatch BeforeTuiStart +
    /// SessionStart, then park) instead of Print/TUI so it never writes to the
    /// extension's stdio. With the cdylib channel gone nothing sets it any more
    /// (the wasm contract has no stdio claim), so this is always `false` in
    /// production.
    pub fn owns_stdio(&self) -> bool {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.owns_stdio())
            .unwrap_or(false)
    }

    /// Whether the session carries no registry entries.
    pub fn is_empty(&self) -> bool {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.is_empty())
            .unwrap_or(true)
    }

    /// B5a: the session's action bridge, if one was threaded in. `kiqi-cli`
    /// recovers this after `AgentHarness::create` succeeds to call
    /// `HarnessActionHost::set_harness` (filling the host cell the bridge's
    /// recovered host reads on the first extension→host action). `None` when no
    /// extensions loaded / tests / `--no-extensions`.
    pub fn action_bridge(&self) -> Option<Arc<ActionBridge>> {
        self.action_bridge.clone()
    }
}
