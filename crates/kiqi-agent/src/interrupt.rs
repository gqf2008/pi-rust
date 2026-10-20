//! Soft interrupt: cancel the *current* tool call(s) / stream without aborting
//! the whole run.
//!
//! A run has one long-lived [`CancellationToken`] (`AgentLoopConfig::signal`).
//! Cancelling it kills the run. A soft interrupt needs the opposite shape: it
//! must reach the action that is in flight right now (one or more tool
//! `execute`s, later also one stream request) and nothing else, so the loop can
//! keep going with the conversation after that action unwinds.
//!
//! [`InterruptControl`] is that targeted handle:
//! - the loop [`install`](InterruptControl::install)s the token of every action
//!   it is about to run, and [`clear`](InterruptControl::clear)s it afterwards;
//! - an outside caller (CLI key binding, harness watchdog) calls
//!   [`soft_interrupt`](InterruptControl::soft_interrupt), which cancels every
//!   installed token and latches a one-shot "this was soft" flag that the loop
//!   owner drains with [`take_soft`](InterruptControl::take_soft).
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
//! [`install`](InterruptControl::install) mints a monotonic [`InstallId`], and
//! `clear` removes exactly the entry carrying that id. This is the
//! `id markup` fallback and it is exact: ids are unique per install, so a late
//! `clear` from an action that already released its own entry is a no-op.
//!
//! ## Parallel tool calls
//!
//! With `ToolExecutionMode::Parallel` several tools are installed concurrently,
//! so the control keeps one registry entry **per install** rather than a single
//! slot. Tool A's `clear` removes only A's entry and leaves B interruptible, no
//! matter which of them finishes first; a soft press cancels every live entry.
//! The loop's phases never mix kinds (one `Stream`, or N `Tool`s), so reporting
//! the *newest* live kind is exact in practice; in the unreachable mixed case
//! the newest install wins, deterministically.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::interrupt_trace;

/// Identity of a single [`InterruptControl::install`] call.
///
/// Handed back to the installer so [`InterruptControl::clear`] can remove
/// exactly its own entry. Monotonic, never reused; ordering by id is install
/// order, which is what [`InterruptControl::current_kind`] uses to pick the
/// newest live action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstallId(u64);

/// Which kind of action an installed entry holds.
///
/// The loop installs one token per in-flight action, and there are two kinds:
/// one tool `execute` (installed by `execute_prepared_tool_call`) and one
/// provider request (installed by the stream path, before the response is
/// folded).
///
/// A soft interrupt reports *what it reached* through this type: "interrupted
/// the tool call" and "interrupted the reply" are different messages to the
/// user, and the note the TUI leaves behind must name the right one. Recording
/// the kind at install time is the only place that knows — by the time the press
/// arrives, the installed token looks identical either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    /// One tool call's `execute`.
    Tool,
    /// One provider request / response stream.
    Stream,
}

struct Current {
    id: InstallId,
    token: CancellationToken,
    kind: ActionKind,
}

/// Cancels every currently installed action on request, and records whether the
/// most recent interrupt was soft.
///
/// Shared between the loop (installs/clears) and whoever triggers the interrupt
/// (an `Arc` handed to the UI layer), so every method is `&self` + interior
/// mutability.
///
/// Semantics of [`soft_interrupt`](Self::soft_interrupt) with nothing installed:
/// the soft flag is latched anyway, the return value is `None`, and
/// [`has_current`](Self::has_current) reports `false`. That split lets a caller
/// distinguish "I cancelled something" from "there was nothing to cancel,
/// remember this for the next action" without a second entry point. A latched
/// flag means the *next* installed action starts already cancelled (see
/// [`install`](Self::install)); it is consumed by
/// [`take_soft`](Self::take_soft), which the action owner is expected to call at
/// an action boundary.
pub struct InterruptControl {
    /// One entry per live action. Parallel tool calls install concurrently, so
    /// this is a registry rather than a single slot: each `clear` removes only
    /// its own entry, and a soft press cancels all of them.
    current: Mutex<Vec<Current>>,
    soft: AtomicBool,
    next_id: AtomicU64,
}

/// RAII guard that releases an [`InterruptControl`] entry on drop.
///
/// Call sites still `clear` explicitly at their own action boundary; the `Drop`
/// is an idempotent backstop for a panicking or aborted action, which would
/// otherwise leave a stale entry behind (a later press would report `Soft` for a
/// dead token and skip arming the latch).
pub(crate) struct InstallSlot<'a> {
    control: &'a InterruptControl,
    id: InstallId,
}

impl InstallSlot<'_> {
    /// The id to pass to [`InterruptControl::clear`] at the explicit boundary.
    pub(crate) fn id(&self) -> InstallId {
        self.id
    }
}

impl Drop for InstallSlot<'_> {
    fn drop(&mut self) {
        self.control.clear(self.id);
    }
}

impl InterruptControl {
    pub fn new() -> Self {
        Self {
            current: Mutex::new(Vec::new()),
            soft: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
        }
    }

    /// Cancel every installed action's token, mark this interrupt as soft, and
    /// report the kind of the newest live action (or `None` if nothing was
    /// installed).
    ///
    /// `cancel()` on an installed token only affects that token and its
    /// descendants, never the run's `signal`, so the run survives. With nothing
    /// installed the flag is still latched (see the type docs); the returned
    /// `None` is what tells the two cases apart. Cancelling and choosing the
    /// reported kind happen under the same lock, so a concurrent `clear` can
    /// never make the verdict name an action that was not actually cancelled.
    pub fn soft_interrupt(&self) -> Option<ActionKind> {
        // Latch first, then cancel: an `install` racing this call either sees
        // the flag and pre-cancels its token, or is already in the registry and
        // gets cancelled below. Either order loses nothing.
        self.soft.store(true, Ordering::SeqCst);
        // Copy what the trace needs and let the guard drop at the end of the
        // block: the audit line is written *after* the lock is released, because
        // a filesystem write under this mutex would block every install/clear of
        // the run on the speed of the disk (see `interrupt_trace`).
        let kind = {
            let guard = self.current.lock().expect("interrupt lock");
            for current in guard.iter() {
                current.token.cancel();
            }
            guard.iter().max_by_key(|c| c.id).map(|c| c.kind)
        };
        interrupt_trace::press(kind);
        kind
    }

    /// Register `token` as a cancellable action; the loop calls this right
    /// before running a tool call (or a stream request), passing which of the
    /// two it is. Each install gets its own registry entry, so peers installed
    /// by a parallel batch stay independently interruptible.
    ///
    /// Returns the id to pass to [`clear`](Self::clear). If a soft interrupt was
    /// latched while nothing was installed, the token is cancelled immediately:
    /// the next action is exactly the thing the caller wanted to interrupt. The
    /// flag itself is *not* consumed here — only
    /// [`take_soft`](Self::take_soft) drains it.
    pub fn install(&self, token: CancellationToken, kind: ActionKind) -> InstallId {
        let id = InstallId(self.next_id.fetch_add(1, Ordering::SeqCst));
        // Read the latch *under* the same lock that `soft_interrupt` takes
        // after storing it. Reading it before the lock left a window in which
        // the press latched while the registry was still empty (so it cancelled
        // nothing) and this install then published an uncancelled token.
        let mut guard = self.current.lock().expect("interrupt lock");
        let latched = self.soft.load(Ordering::SeqCst);
        if latched {
            token.cancel();
        }
        guard.push(Current { id, token, kind });
        // Release before tracing: `install` runs once per action of every run, so
        // a slow log path here must not serialize the agent loop.
        drop(guard);
        if latched {
            interrupt_trace::latched_cancelled(kind);
        }
        id
    }

    /// [`install`](Self::install), plus an RAII backstop that clears the entry
    /// when the returned guard is dropped.
    ///
    /// Explicit [`clear`](Self::clear) is still expected at the action boundary
    /// (the stream path must release between folding and classification); the
    /// guard's `Drop` only matters when an action panics or is cancelled without
    /// reaching that boundary.
    pub(crate) fn install_slot(
        &self,
        token: CancellationToken,
        kind: ActionKind,
    ) -> InstallSlot<'_> {
        InstallSlot {
            control: self,
            id: self.install(token, kind),
        }
    }

    /// Release the entry carrying `id`.
    ///
    /// A no-op when it was already removed: with parallel tool execution the
    /// finishing order has nothing to do with the installing order, and a late
    /// `clear` must never touch a peer's entry.
    pub fn clear(&self, id: InstallId) {
        let mut guard = self.current.lock().expect("interrupt lock");
        guard.retain(|c| c.id != id);
    }

    /// Consume the soft-interrupt flag. Returns `true` if the most recent
    /// interrupt was soft. One-shot: the next call returns `false` unless
    /// another `soft_interrupt` happened in between.
    pub fn take_soft(&self) -> bool {
        self.soft.swap(false, Ordering::SeqCst)
    }

    /// Which action (of the live ones) [`soft_interrupt`] would name right now:
    /// the newest install.
    ///
    /// `None` while idle: between turns, or with a latched soft interrupt and
    /// nothing running. A caller that only wants "is there anything to cancel"
    /// uses [`has_current`](Self::has_current); one that reports *what* it hit
    /// (the TUI note, the harness verdict) uses this.
    ///
    /// [`soft_interrupt`]: Self::soft_interrupt
    pub fn current_kind(&self) -> Option<ActionKind> {
        self.current
            .lock()
            .expect("interrupt lock")
            .iter()
            .max_by_key(|c| c.id)
            .map(|c| c.kind)
    }

    /// Whether there is an installed action that [`soft_interrupt`] could
    /// cancel right now. `false` while idle (between turns, or a latched soft
    /// interrupt with nothing running).
    ///
    /// Defined through [`current_kind`](Self::current_kind) so the "is there
    /// something?" and "what is it?" answers can never disagree: one lock, one
    /// definition of the registry.
    ///
    /// [`soft_interrupt`]: Self::soft_interrupt
    pub fn has_current(&self) -> bool {
        self.current_kind().is_some()
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
        let _id = ctl.install(token.clone(), ActionKind::Tool);
        assert!(ctl.has_current());
        assert!(!token.is_cancelled());

        assert_eq!(ctl.soft_interrupt(), Some(ActionKind::Tool));

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
        let _id = ctl.install(child.clone(), ActionKind::Stream);

        assert_eq!(ctl.soft_interrupt(), Some(ActionKind::Stream));

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

        let id_a = ctl.install(a.clone(), ActionKind::Tool);
        let id_b = ctl.install(b.clone(), ActionKind::Tool);
        assert_ne!(id_a, id_b);

        // A finishes second and must not evict B.
        ctl.clear(id_a);
        assert!(ctl.has_current(), "B is still installed");

        assert_eq!(ctl.soft_interrupt(), Some(ActionKind::Tool));
        assert!(b.is_cancelled(), "B is still the interruptible action");
        assert!(
            !a.is_cancelled(),
            "A was already released, nobody cancelled it"
        );

        ctl.clear(id_b);
        assert!(!ctl.has_current());
    }

    #[test]
    fn soft_interrupt_cancels_every_live_action() {
        // A parallel tool batch installs one entry per tool. One press must
        // reach all of them, not just the newest: the user pressed Esc to stop
        // what is running, and every tool of the batch is running.
        let ctl = InterruptControl::new();
        let slow = CancellationToken::new();
        let fast = CancellationToken::new();
        let slow_id = ctl.install(slow.clone(), ActionKind::Tool);
        let fast_id = ctl.install(fast.clone(), ActionKind::Tool);

        assert_eq!(ctl.soft_interrupt(), Some(ActionKind::Tool));

        assert!(slow.is_cancelled(), "the older action is cancelled too");
        assert!(fast.is_cancelled());
        assert!(ctl.has_current(), "both actions are still in flight");
        assert!(ctl.take_soft(), "the batch was soft-interrupted");

        ctl.clear(slow_id);
        assert!(ctl.has_current(), "the peer survives its own clear");
        ctl.clear(fast_id);
        assert!(!ctl.has_current());
    }

    #[test]
    fn clear_of_a_finished_parallel_action_leaves_the_others_interruptible() {
        // The exact sequence of a parallel batch where the second-installed
        // tool finishes first. With a single shared slot (the old design) its
        // `clear` emptied the slot while the first tool was still running, so
        // the press found nothing and the surviving tool never died.
        let ctl = InterruptControl::new();
        let slow = CancellationToken::new();
        let fast = CancellationToken::new();
        let slow_id = ctl.install(slow.clone(), ActionKind::Tool);
        let fast_id = ctl.install(fast.clone(), ActionKind::Tool);

        ctl.clear(fast_id);
        assert_eq!(
            ctl.current_kind(),
            Some(ActionKind::Tool),
            "the still-running peer is the interrupt target"
        );
        assert!(ctl.has_current());

        assert_eq!(ctl.soft_interrupt(), Some(ActionKind::Tool));
        assert!(
            slow.is_cancelled(),
            "the surviving parallel tool is cancelled"
        );
        assert!(
            !fast.is_cancelled(),
            "the action that already finished is not cancelled"
        );
        assert!(ctl.take_soft());
        ctl.clear(slow_id);
        assert!(!ctl.has_current());
    }

    #[test]
    fn clear_is_idempotent() {
        let ctl = InterruptControl::new();
        let token = CancellationToken::new();
        let id = ctl.install(token.clone(), ActionKind::Tool);

        ctl.clear(id);
        ctl.clear(id);
        assert!(!ctl.has_current());
        assert!(!token.is_cancelled());
    }

    #[test]
    fn install_slot_clears_on_drop() {
        let ctl = InterruptControl::new();
        let token = CancellationToken::new();
        {
            let _slot = ctl.install_slot(token.clone(), ActionKind::Tool);
            assert!(ctl.has_current(), "the slot is installed");
        }
        assert!(!ctl.has_current(), "dropping the guard releases the entry");
        assert!(!token.is_cancelled(), "dropping is not cancelling");
    }

    #[test]
    fn current_kind_reports_which_action_is_installed() {
        // The press note / harness verdict names the action it reached, so the
        // kind has to follow the live registry rather than any earlier one.
        let ctl = InterruptControl::new();
        assert_eq!(ctl.current_kind(), None, "idle: nothing to name");

        let tool = CancellationToken::new();
        let tool_id = ctl.install(tool.clone(), ActionKind::Tool);
        assert_eq!(ctl.current_kind(), Some(ActionKind::Tool));
        assert!(ctl.has_current(), "has_current must agree with the kind");

        // A newer install is reported while it is live: a stream request
        // starting while a parallel tool batch is still running reports
        // `Stream` (mixed kinds never happen in the loop, but the answer is
        // deterministic when they do).
        let stream = CancellationToken::new();
        let stream_id = ctl.install(stream.clone(), ActionKind::Stream);
        assert_eq!(
            ctl.current_kind(),
            Some(ActionKind::Stream),
            "the kind tracks the newest install, not the first one"
        );

        // Clearing the older action must not lie about the kind.
        ctl.clear(tool_id);
        assert_eq!(ctl.current_kind(), Some(ActionKind::Stream));

        ctl.clear(stream_id);
        assert_eq!(ctl.current_kind(), None);
        assert!(!ctl.has_current());
    }

    #[test]
    fn take_soft_is_one_shot() {
        let ctl = InterruptControl::new();
        let token = CancellationToken::new();
        let _id = ctl.install(token.clone(), ActionKind::Tool);

        ctl.soft_interrupt();
        assert!(ctl.take_soft());
        assert!(!ctl.take_soft());
        assert!(!ctl.take_soft());
    }

    #[test]
    fn soft_interrupt_without_current_latches_but_reports_no_current() {
        let ctl = InterruptControl::new();

        assert_eq!(ctl.soft_interrupt(), None, "nothing was there to name");

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
        let _id = ctl.install(next.clone(), ActionKind::Stream);

        assert!(
            next.is_cancelled(),
            "the action installed after a latched interrupt starts cancelled"
        );
        assert!(ctl.take_soft(), "installing does not drain the flag");

        let later = CancellationToken::new();
        let _id2 = ctl.install(later.clone(), ActionKind::Tool);
        assert!(!later.is_cancelled());
    }

    #[test]
    fn latched_press_pre_cancels_every_action_of_a_parallel_batch() {
        // A press that landed with nothing installed is a promise to interrupt
        // the next step. With a registry every tool of the next batch sees the
        // latch before the first boundary drains it — the old single slot had
        // the same (documented) behaviour, and the registry must preserve it.
        let ctl = InterruptControl::new();
        ctl.soft_interrupt();

        let first = CancellationToken::new();
        let second = CancellationToken::new();
        let first_id = ctl.install(first.clone(), ActionKind::Tool);
        let second_id = ctl.install(second.clone(), ActionKind::Tool);

        assert!(first.is_cancelled());
        assert!(second.is_cancelled());
        assert!(
            ctl.take_soft(),
            "the latch is drained at the action boundary"
        );

        ctl.clear(first_id);
        ctl.clear(second_id);
    }

    #[test]
    fn installed_token_is_cleared_before_a_new_soft_interrupt() {
        let ctl = InterruptControl::new();
        let first = CancellationToken::new();
        let id = ctl.install(first.clone(), ActionKind::Tool);
        ctl.clear(id);

        assert_eq!(ctl.soft_interrupt(), None);

        assert!(!first.is_cancelled(), "released actions are not cancelled");
        assert!(ctl.take_soft(), "the flag is still latched");
    }
}
