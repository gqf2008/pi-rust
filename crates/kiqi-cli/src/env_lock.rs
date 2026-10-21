//! One lock for every read and write of the process-global environment.
//!
//! `std::env::set_var`/`var` act on a single cell per process, so two code paths
//! that touch the same variable race even when they live in different modules and
//! neither is "the owner". Inside `ki` this bites in two ways:
//!
//! * **Tests.** `cargo test` runs them on many threads. A test that snapshots
//!   `KI_AGENT_ID` and asserts on it is racing any other test that reaches
//!   `set_active_session`, which republishes both id variables.
//! * **Production.** The harness activates a session on a background task, so a
//!   reader on another task can observe the value mid-rewrite.
//!
//! Historically only the *tests* took a lock (`config::test_support::env_lock`),
//! which serialized test-against-test but not test-against-the-product-code it
//! calls. `plugin_session_actions_reattach_the_signal_host` reached
//! `set_active_session` without the lock and rewrote `KI_AGENT_ID` under a
//! sibling test's assertion — measured at 15/40 runs failing with
//! `--test-threads=16` and 0/20 with a single thread. Every writer therefore
//! goes through [`with_env_lock`], and the tests hold the same lock, so the two
//! directions finally share one mutex.
//!
//! Two properties this type has to keep, both load-bearing:
//!
//! * **Re-entrant per thread.** Tests take the lock for the whole test and then
//!   call functions that also take it (assert-on-publish, assert-on-swap). A
//!   plain `Mutex` would deadlock on the first such call, so a thread that
//!   already holds it re-enters without blocking.
//! * **Poison-tolerant.** A panic while holding it used to turn one real failure
//!   into 93: the 33 call sites did `.lock().unwrap()` and every later test died
//!   with `PoisonError` instead of its own result. A poisoned lock says nothing
//!   about the environment, so it is recovered and used anyway.
//!
//! This is deliberately *one* lock for the whole environment rather than one per
//! variable: the failure mode being prevented is two variables observed
//! mid-update, and per-variable locks would let a `KI_SESSION_ID`/`KI_AGENT_ID`
//! pair be torn. It is uncontended in single-threaded production paths, so the
//! cost is a lock/unlock with nothing waiting.

use std::cell::Cell;
use std::sync::{Mutex, MutexGuard, OnceLock};

fn mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

thread_local! {
    /// How many times the current thread holds the lock. Zero means "not held".
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Held for as long as one thread is inside the environment.
///
/// The outer `MutexGuard` is `None` for a re-entrant acquisition, which is what
/// keeps the nesting affordable: the thread already owns the mutex.
#[must_use = "the environment is only protected while the guard is alive"]
pub(crate) struct EnvGuard {
    inner: Option<MutexGuard<'static, ()>>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        // `inner` is dropped after this line, releasing the mutex (if this was
        // the outermost acquisition) once the depth is already back down.
    }
}

/// The process-wide environment lock.
pub(crate) struct EnvLock;

impl EnvLock {
    /// Acquire it, re-entering when this thread already holds it.
    ///
    /// Never returns an error: a poisoned lock is recovered, because a panic
    /// elsewhere says nothing about the environment's contents.
    pub(crate) fn lock(&self) -> EnvGuard {
        let held = DEPTH.with(Cell::get);
        if held > 0 {
            DEPTH.with(|depth| depth.set(held + 1));
            return EnvGuard { inner: None };
        }
        let inner = mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        DEPTH.with(|depth| depth.set(1));
        EnvGuard { inner: Some(inner) }
    }
}

/// The one lock, for both the tests (`config::test_support::env_lock`) and the
/// code paths that publish or read the environment.
pub(crate) fn env_lock() -> &'static EnvLock {
    static LOCK: OnceLock<EnvLock> = OnceLock::new();
    LOCK.get_or_init(|| EnvLock)
}

/// Run `f` while holding the environment lock.
///
/// Every `std::env::set_var` that another thread could observe belongs inside
/// this, especially the `KI_*` pair that names the active session.
pub(crate) fn with_env_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = env_lock().lock();
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The property the whole design rests on: a test may hold the lock and then
    /// call product code that takes it again. A non-re-entrant mutex would hang
    /// here rather than fail, so the assertion is that control returns at all.
    #[test]
    fn a_thread_that_already_holds_it_re_enters() {
        let _outer = env_lock().lock();
        let value = with_env_lock(|| with_env_lock(|| 7));
        assert_eq!(value, 7);
    }

    /// Releasing one nesting level must not release the lock for the others.
    #[test]
    fn nesting_releases_one_level_at_a_time() {
        let outer = env_lock().lock();
        let inner = env_lock().lock();
        drop(inner);
        // Still held by `outer`: another thread must not get in.
        let seen = Arc::new(AtomicUsize::new(0));
        let flag = Arc::clone(&seen);
        let handle = std::thread::spawn(move || {
            // Blocks until `outer` is dropped, then runs.
            with_env_lock(|| flag.store(1, Ordering::SeqCst));
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "the outer guard still holds it"
        );
        drop(outer);
        handle.join().unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    /// A panic inside the lock must not hide later failures behind `PoisonError`;
    /// that amplification is what turned one real failure into 93.
    #[test]
    fn a_poisoned_lock_is_still_usable() {
        let panicked = std::thread::spawn(|| {
            let _guard = env_lock().lock();
            panic!("deliberate: poison the environment lock");
        });
        assert!(panicked.join().is_err());
        // Would panic with `PoisonError` if `lock` propagated poisoning.
        assert_eq!(with_env_lock(|| 3), 3);
    }

    /// Concurrent writers must not interleave. Each iteration stores its own
    /// value and reads it back while holding the lock; a lost update shows up as
    /// a mismatch instead of a silent pass.
    #[test]
    fn concurrent_writers_see_their_own_value() {
        let name = "KI_TEST_ENV_LOCK_PROBE";
        std::env::set_var(name, "start");
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                std::thread::spawn(move || {
                    for round in 0..50 {
                        let value = format!("{worker}-{round}");
                        with_env_lock(|| {
                            std::env::set_var(name, &value);
                            let read = std::env::var(name).unwrap();
                            assert_eq!(read, value, "another writer interleaved");
                        });
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        std::env::remove_var(name);
    }
}
