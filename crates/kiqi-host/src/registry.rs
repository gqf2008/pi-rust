//! The session's extension-state snapshot.
//!
//! Historically this module accumulated native (cdylib) registrations during an
//! extension's `kiqi_extension_register` call and snapshotted them for the host
//! session. The cdylib channel was retired on 2026-10-26 and the C-ABI
//! registration machine that fed it was removed in the pure-serde event-bus
//! batch (2026-10-27, `docs/design/extension-abi.md` §6.49). What survives is
//! the piece the dispatch spine still uses: the shared **staleness flag** a
//! `/reload` flips so a swapped-out session stops dispatching.
//!
//! Extensions themselves live in the wasm backend and receive events through
//! the [`compiled`](crate::compiled) sink; there is no native registry any more.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// An immutable snapshot of a host session's extension state.
///
/// The only state left is the `active` flag shared with the session that built
/// it. It is deliberately cheap to clone behind an `Arc` (the event emitter and
/// lifecycle dispatchers hold one for the session's lifetime).
pub struct RegistrySnapshot {
    active: Arc<AtomicBool>,
}

impl RegistrySnapshot {
    /// A fresh, active snapshot. Never carries native registrations — there is
    /// no native registration channel any more.
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
        }
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
}
