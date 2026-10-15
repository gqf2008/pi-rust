//! Cancellation token shared between a tool's `cancel`/`destroy`, its worker
//! thread, and — for `dispatch` — the per-node workers it starts.
//!
//! A tool call runs on a worker thread so `poll` can stay non-blocking. The host
//! signals cancellation through the ABI `cancel` entry point, which flips this
//! flag; the worker observes it and kills whatever child process it is waiting
//! on. A node token can additionally be cancelled on its own, so a mid-wave
//! `plan cancel` can stop one node without stopping the whole call.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct Control {
    primary: Arc<AtomicBool>,
    /// Cancelled whenever the owning call is; checked alongside `primary`.
    linked: Option<Arc<AtomicBool>>,
}

impl Control {
    pub fn cancel(&self) {
        self.primary.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        if self.primary.load(Ordering::Acquire) {
            return true;
        }
        if let Some(flag) = &self.linked {
            if flag.load(Ordering::Acquire) {
                // Latch: once a parent's abort is observed it must not be
                // un-observed. The run-start reset clears `HOST_ABORT` for the
                // *next* run; a node of the aborted run that had not polled yet
                // stays cancelled instead of resuming under the new run.
                self.primary.store(true, Ordering::Release);
                return true;
            }
        }
        false
    }

    /// True when both tokens share the same primary flag — the identity check
    /// that lets a worker unregister only its own entry from the running map
    /// (a same-id node re-dispatched while the old one winds down must keep
    /// its own registration).
    pub fn same_token_as(&self, other: &Control) -> bool {
        Arc::ptr_eq(&self.primary, &other.primary)
    }

    /// `Err` once cancelled — the worker's cheap checkpoint.
    pub fn check(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err("cancelled".into())
        } else {
            Ok(())
        }
    }

    /// A token sharing `flag`, so a caller can expose one cancellation signal to
    /// many holders.
    pub fn from_flag(flag: Arc<AtomicBool>) -> Control {
        Control {
            primary: flag,
            linked: None,
        }
    }

    /// A token cancelled by its own [`cancel`](Self::cancel) *or* whenever
    /// `parent` is cancelled.
    pub fn child_of(parent: &Control) -> Control {
        Control {
            primary: Arc::new(AtomicBool::new(false)),
            linked: Some(Arc::clone(&parent.primary)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node that had not polled yet when the run-start reset cleared the
    /// parent flag must still stay cancelled once it DID observe the abort —
    /// the observation latches into the child's own flag.
    #[test]
    fn an_observed_parent_cancel_latches() {
        let flag = Arc::new(AtomicBool::new(false));
        let parent = Control::from_flag(Arc::clone(&flag));
        let child = Control::child_of(&parent);

        flag.store(true, Ordering::Release);
        assert!(child.is_cancelled(), "the abort is observed");
        // The next run's start clears the parent flag.
        flag.store(false, Ordering::Release);
        assert!(
            child.is_cancelled(),
            "the observed abort stays latched in the child"
        );
    }

    #[test]
    fn same_token_matches_only_the_same_flag() {
        let a = Control::from_flag(Arc::new(AtomicBool::new(false)));
        let a_clone = a.clone();
        let b = Control::from_flag(Arc::new(AtomicBool::new(false)));
        assert!(a.same_token_as(&a_clone));
        assert!(!a.same_token_as(&b));
    }

    #[test]
    fn a_child_is_cancelled_by_its_parent_or_on_its_own() {
        let parent = Control::default();
        let child = Control::child_of(&parent);
        assert!(!child.is_cancelled());
        assert!(!parent.is_cancelled());

        parent.cancel();
        assert!(child.is_cancelled(), "parent cancel reaches the child");

        // The reverse does not hold: cancelling a node token leaves the call alive.
        let parent = Control::default();
        let child = Control::child_of(&parent);
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());
    }
}
