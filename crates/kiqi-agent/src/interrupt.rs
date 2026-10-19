//! Soft interrupt: cancel the *current* tool call / stream without aborting the
//! whole run.
//!
//! A run has one long-lived [`CancellationToken`] (`AgentLoopConfig::signal`).
//! Cancelling it kills the run. A soft interrupt needs the opposite shape: it
//! must reach the action that is in flight right now (one tool `execute`, later
//! also one stream request) and nothing else, so the loop can keep going with
//! the conversation after that action unwinds.
//!
//! [`InterruptControl`] is that targeted handle:
//! - the loop [`install`](InterruptControl::install)s the token of the action it
//!   is about to run, and [`clear`](InterruptControl::clear)s it afterwards;
//! - an outside caller (CLI key binding, harness watchdog) calls
//!   [`soft_interrupt`](InterruptControl::soft_interrupt), which cancels only
//!   the installed token and latches a one-shot "this was soft" flag that the
//!   loop owner drains with [`take_soft`](InterruptControl::take_soft).
//!
//! The two tokens never alias each other: actions are installed with
//! `signal.child_token()`, so cancelling a child can't leak up into the run
//! signal. That is what makes a soft interrupt resumable.
//!
//! ## Why install ids instead of comparing tokens
//!
//! The natural check for `clear` is "is the token you finished still the one
//! installed?". tokio-util 0.7's [`CancellationToken`] does not support that:
//! it exposes `new` / `child_token` / `cancel` / `is_cancelled` / `cancelled` /
//! `cancelled_owned` / `drop_guard` / `drop_guard_ref` and nothing else — no
//! `PartialEq`, no `Hash`, no pointer accessor, and its inner `TreeNode` is
//! private, so `Arc::ptr_eq`-style identity is unreachable from outside the
//! crate. Rather than adding a dependency or unsafe pointer surgery, every
//! [`install`](InterruptControl::install) mints a monotonic [`InstallId`] and
//! `clear` only clears when that id is still the installed one. This is the
//! `id markup` fallback and it is exact: ids are unique per install, so a late
//! `clear` from an action that a newer install already displaced is a no-op.
//!
//! This matters with `ToolExecutionMode::Parallel`, where several tools install
//! concurrently: tool A must not wipe out tool B's slot when A finishes second.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Identity of a single [`InterruptControl::install`] call.
///
/// Handed back to the installer so [`InterruptControl::clear`] can tell "my
/// slot" from "someone else's slot". Monotonic, never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InstallId(u64);

struct Current {
    id: InstallId,
    token: CancellationToken,
}

/// Cancels the currently installed action on request, and records whether the
/// most recent interrupt was soft.
///
/// Shared between the loop (installs/clears) and whoever triggers the interrupt
/// (an `Arc` handed to the UI layer), so every method is `&self` + interior
/// mutability.
///
/// Semantics of [`soft_interrupt`](Self::soft_interrupt) with nothing installed:
/// the soft flag is latched anyway, and [`has_current`](Self::has_current)
/// reports `false`. That split lets a caller distinguish "I cancelled something"
/// from "there was nothing to cancel, remember this for the next action" without
/// a second entry point. A latched flag means the *next* installed action starts
/// already cancelled (see [`install`](Self::install)); it is consumed by
/// [`take_soft`](Self::take_soft), which the action owner is expected to call at
/// an action boundary.
pub struct InterruptControl {
    current: Mutex<Option<Current>>,
    soft: AtomicBool,
    next_id: AtomicU64,
}

impl InterruptControl {
    pub fn new() -> Self {
        Self {
            current: Mutex::new(None),
            soft: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
        }
    }

    /// Cancel the current action's token and mark this interrupt as soft.
    ///
    /// `cancel()` on the installed token only affects that token and its
    /// descendants, never the run's `signal`, so the run survives. With nothing
    /// installed the flag is still latched (see the type docs); `has_current()`
    /// is what tells the two cases apart.
    pub fn soft_interrupt(&self) {
        // Latch first, then cancel: an `install` racing this call either sees
        // the flag and pre-cancels its token, or is already in the slot and gets
        // cancelled below. Either order loses nothing.
        self.soft.store(true, Ordering::SeqCst);
        let guard = self.current.lock().expect("interrupt lock");
        if let Some(current) = guard.as_ref() {
            current.token.cancel();
        }
    }

    /// Register `token` as the current cancellable action; the loop calls this
    /// right before running a tool call (or a stream request).
    ///
    /// Returns the id to pass to [`clear`](Self::clear). If a soft interrupt was
    /// latched while nothing was installed, the token is cancelled immediately:
    /// the next action is exactly the thing the caller wanted to interrupt. The
    /// flag itself is *not* consumed here — only
    /// [`take_soft`](Self::take_soft) drains it.
    pub fn install(&self, token: CancellationToken) -> InstallId {
        let id = InstallId(self.next_id.fetch_add(1, Ordering::SeqCst));
        if self.soft.load(Ordering::SeqCst) {
            token.cancel();
        }
        let mut guard = self.current.lock().expect("interrupt lock");
        *guard = Some(Current { id, token });
        id
    }

    /// Release the slot, but only if `id` is still the installed one.
    ///
    /// A no-op when a newer install displaced us: with parallel tool execution
    /// the finishing order has nothing to do with the installing order, and the
    /// later action's token must stay interruptible.
    pub fn clear(&self, id: InstallId) {
        let mut guard = self.current.lock().expect("interrupt lock");
        if guard.as_ref().map(|c| c.id) == Some(id) {
            *guard = None;
        }
    }

    /// Consume the soft-interrupt flag. Returns `true` if the most recent
    /// interrupt was soft. One-shot: the next call returns `false` unless
    /// another `soft_interrupt` happened in between.
    pub fn take_soft(&self) -> bool {
        self.soft.swap(false, Ordering::SeqCst)
    }

    /// Whether there is an installed action that [`soft_interrupt`] could
    /// cancel right now. `false` while idle (between turns, or a latched soft
    /// interrupt with nothing running).
    ///
    /// [`soft_interrupt`]: Self::soft_interrupt
    pub fn has_current(&self) -> bool {
        self.current.lock().expect("interrupt lock").is_some()
    }
}

impl Default for InterruptControl {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for InterruptControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterruptControl")
            .field("has_current", &self.has_current())
            .field("soft", &self.soft.load(Ordering::SeqCst))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_then_soft_interrupt_cancels_that_token() {
        let ctl = InterruptControl::new();
        let token = CancellationToken::new();
        assert!(!ctl.has_current());
        let _id = ctl.install(token.clone());
        assert!(ctl.has_current());
        assert!(!token.is_cancelled());

        ctl.soft_interrupt();

        assert!(token.is_cancelled(), "installed token must be cancelled");
        assert!(ctl.has_current(), "the action is still in flight");
        assert!(ctl.take_soft(), "the interrupt was soft");
        assert!(!ctl.take_soft(), "take_soft is one-shot");
    }

    #[test]
    fn soft_interrupt_does_not_cancel_the_parent_signal() {
        let run_signal = CancellationToken::new();
        let ctl = InterruptControl::new();
        let child = run_signal.child_token();
        let _id = ctl.install(child.clone());

        ctl.soft_interrupt();

        assert!(child.is_cancelled());
        assert!(
            !run_signal.is_cancelled(),
            "a soft interrupt must not kill the run"
        );
    }

    #[test]
    fn clear_only_clears_its_own_install() {
        let ctl = InterruptControl::new();
        let (a, b) = (CancellationToken::new(), CancellationToken::new());

        let id_a = ctl.install(a.clone());
        let id_b = ctl.install(b.clone());
        assert_ne!(id_a, id_b);

        // A finishes second and must not evict B.
        ctl.clear(id_a);
        assert!(ctl.has_current(), "B is still installed");

        ctl.soft_interrupt();
        assert!(b.is_cancelled(), "B is still the interruptible action");
        assert!(
            !a.is_cancelled(),
            "A was already released, nobody cancelled it"
        );

        ctl.clear(id_b);
        assert!(!ctl.has_current());
    }

    #[test]
    fn clear_is_idempotent() {
        let ctl = InterruptControl::new();
        let token = CancellationToken::new();
        let id = ctl.install(token.clone());

        ctl.clear(id);
        ctl.clear(id);
        assert!(!ctl.has_current());
        assert!(!token.is_cancelled());
    }

    #[test]
    fn take_soft_is_one_shot() {
        let ctl = InterruptControl::new();
        let token = CancellationToken::new();
        let _id = ctl.install(token.clone());

        ctl.soft_interrupt();
        assert!(ctl.take_soft());
        assert!(!ctl.take_soft());
        assert!(!ctl.take_soft());
    }

    #[test]
    fn soft_interrupt_without_current_latches_but_reports_no_current() {
        let ctl = InterruptControl::new();

        ctl.soft_interrupt();

        assert!(
            !ctl.has_current(),
            "nothing in flight: caller can tell it was `NothingToInterrupt`"
        );
        assert!(ctl.take_soft(), "the flag is latched anyway");
        assert!(!ctl.take_soft());
    }

    #[test]
    fn latched_soft_interrupt_cancels_the_next_install() {
        let ctl = InterruptControl::new();
        ctl.soft_interrupt();

        let next = CancellationToken::new();
        let _id = ctl.install(next.clone());

        assert!(
            next.is_cancelled(),
            "the action installed after a latched interrupt starts cancelled"
        );
        assert!(ctl.take_soft(), "installing does not drain the flag");

        let later = CancellationToken::new();
        let _id2 = ctl.install(later.clone());
        assert!(!later.is_cancelled());
    }

    #[test]
    fn installed_token_is_cleared_before_a_new_soft_interrupt() {
        let ctl = InterruptControl::new();
        let first = CancellationToken::new();
        let id = ctl.install(first.clone());
        ctl.clear(id);

        ctl.soft_interrupt();

        assert!(!first.is_cancelled(), "released actions are not cancelled");
        assert!(ctl.take_soft(), "the flag is still latched");
    }
}
