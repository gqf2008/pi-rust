//! Exit the process when the controlling terminal goes away.
//!
//! crossterm 0.27's Unix event source spins forever when a read on the tty
//! fails with anything other than `WouldBlock`/`Interrupted`
//! (`crossterm-0.27.0/src/event/source/unix/mio.rs`, the `TTY_TOKEN` arm of
//! `try_read`): a hung-up terminal makes `read` return `EIO`, that inner loop
//! re-reads immediately, and `event::poll` never returns. The thread that would
//! notice the hangup is therefore the thread that is wedged inside the library.
//!
//! This watchdog runs on its own thread, watches stdin for `POLLHUP`, and exits
//! the process once the terminal is gone. It is the only exit that can unblock
//! the wedged reader, and it also covers the case where `SIGHUP` is never
//! delivered (a background or orphaned process group), which would otherwise
//! spin at ~a third of a core forever — see the issue report that prompted it.

/// How often the watchdog checks stdin. Small enough to exit promptly, cheap
/// enough to be invisible: one non-blocking `poll(2)` per tick.
#[cfg(unix)]
const INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Start the watchdog. Idempotent, and a no-op when stdin is not a terminal
/// (nothing to watch: a pipe or `/dev/null` never reports a hangup).
pub fn spawn() {
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        use std::sync::OnceLock;

        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            if !std::io::stdin().is_terminal() {
                return;
            }
            let _ = std::thread::Builder::new()
                .name("ki-tty-hangup".to_string())
                .spawn(watch);
        });
    }
}

#[cfg(unix)]
fn watch() -> ! {
    loop {
        std::thread::sleep(INTERVAL);
        if stdin_hung_up() {
            restore_terminal();
            // The crossterm reader is wedged inside the library and cannot be
            // joined; exiting the process is the only way out.
            std::process::exit(0);
        }
    }
}

/// True when stdin reports `POLLHUP`/`POLLERR`/`POLLNVAL`.
#[cfg(unix)]
fn stdin_hung_up() -> bool {
    use std::os::fd::AsFd;

    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    let stdin = std::io::stdin();
    let mut fds = [PollFd::new(
        stdin.as_fd(),
        PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL,
    )];
    match poll(&mut fds, PollTimeout::ZERO) {
        // `revents()` is `None` when nothing happened, so no intersect check is
        // needed for the idle case.
        Ok(_) => fds[0].revents().is_some_and(|events| {
            events.intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL)
        }),
        // A failed poll is not evidence of a hangup; keep watching.
        Err(_) => false,
    }
}

/// Best-effort terminal cleanup. The terminal is usually already gone, so every
/// step is allowed to fail.
#[cfg(unix)]
fn restore_terminal() {
    use std::io::Write;

    let _ = crossterm::terminal::disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{}", crossterm::cursor::Show);
    let _ = crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen);
    let _ = stdout.flush();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const CHILD_ENV: &str = "KI_TTY_HANGUP_WATCHDOG_CHILD";

    /// Regression test for "orphaned `ki` spins after its terminal is gone".
    ///
    /// The process under test re-executes this test binary with [`CHILD_ENV`]
    /// set; that half starts the watchdog and then wedges (the real process is
    /// wedged inside crossterm, but a sleep is enough to reproduce "does not
    /// exit on its own"). The parent hands it a pty as stdin, closes the master
    /// to hang it up, and asserts the child exits instead of running forever.
    ///
    /// SIGHUP is not involved: the child never gets one (it is not a session
    /// leader behind this pty), which is exactly the reported situation.
    #[test]
    fn exits_when_the_controlling_terminal_hangs_up() {
        if std::env::var_os(CHILD_ENV).is_some() {
            spawn();
            std::thread::sleep(Duration::from_secs(3600));
            return;
        }

        let pty = nix::pty::openpty(None, None).expect("openpty");
        // `openpty` does not set FD_CLOEXEC, and `Command` does not close fds the
        // parent did not create with CLOEXEC: without this the child would
        // inherit the master, and closing the parent's copy would not hang up
        // the slave.
        {
            use std::os::fd::AsRawFd;
            nix::fcntl::fcntl(
                pty.master.as_raw_fd(),
                nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
            )
            .expect("set FD_CLOEXEC on the pty master");
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tui_hangup::tests::exits_when_the_controlling_terminal_hangs_up",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .stdin(Stdio::from(pty.slave))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the watchdog child");

        // Let the child reach `spawn()` before the hangup. The watchdog already
        // catches a hangup that predates it on its first tick, so this is only
        // about keeping the test deterministic.
        std::thread::sleep(Duration::from_millis(1500));

        // Closing the master hangs up the slave the child watches.
        drop(pty.master);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => {
                    assert!(status.success(), "watchdog child exited with {status}");
                    return;
                }
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child kept running after its terminal hung up");
                }
            }
        }
    }
}
