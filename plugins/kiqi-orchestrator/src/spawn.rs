//! Subprocess runner for a single delegation.
//!
//! The runtime already parallelises tool calls (`kiqi-agent`'s
//! `execute_tool_calls_parallel`) and feeds every tool result back into the
//! orchestrator's context, so this module only has to launch, bound, and collect
//! one child `ki`.
//!
//! The task travels on **stdin**, not as a positional argument: `ki -p`
//! consumes the next non-flag token as its prompt, so a task beginning with `-`
//! would be parsed as a flag. Piped stdin becomes the initial prompt
//! (`kiqi-cli::app::build_initial_message`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use wait_timeout::ChildExt;

use crate::control::Control;

/// Tools this package registers. Excluded from the child's tool set so a
/// subagent cannot recursively orchestrate (design §8 "递归守卫").
pub const EXCLUDED_TOOLS: &str = "subagent,plan,dispatch";

/// Upper bound on the text fed back to the orchestrator (design §8 "输出截断").
pub const MAX_OUTPUT_BYTES: usize = 40_000;

/// Turn budget env the host already enforces (`kiqi-harness::run_budget`).
pub const MAX_TURNS_ENV: &str = "KI_MAX_TURNS_PER_RUN";

/// Marker set on the child so the orchestrator guidance is not injected into a
/// subagent (which does not have `plan`/`dispatch`).
pub const SUBAGENT_ENV: &str = "KI_ORCHESTRATOR_SUBAGENT";

/// Heartbeat period handed to the child (`KI_HEARTBEAT_SECS`).
///
/// `ki -p` prints its final text only when the run ends, so stdout carries no
/// liveness signal at all — the child's stderr heartbeat (implemented in
/// `kiqi-cli`'s print mode) is what keeps the parent's activity clock moving
/// through a long tool call. Kept well below any plausible stall window.
pub const CHILD_HEARTBEAT_SECS: u64 = 15;

/// Env name the child reads to turn its heartbeat on.
pub const HEARTBEAT_SECS_ENV: &str = "KI_HEARTBEAT_SECS";

/// Env gate for the subagent stage log: `KI_DEBUG_SUBAGENT=1` appends to
/// `<temp>/kiqi-subagent.log`, any other value is used as the path.
pub const DEBUG_LOG_ENV: &str = "KI_DEBUG_SUBAGENT";

/// How often the wait loop snapshots the child's progress into the stage log.
const STAGE_LOG_EVERY: Duration = Duration::from_secs(30);

/// How often the wait loop re-checks cancellation/timeout. Bounds how long a
/// cancelled call can keep a child alive.
const POLL_SLICE: Duration = Duration::from_millis(100);

/// Which pipe a byte count belongs to.
#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// Last-activity clock shared by both pipe readers and the wait loop.
///
/// Stdout silence alone is *not* a stall signal (`ki -p` writes nothing until
/// the run ends), but *any* byte on either pipe is progress: the child's
/// `KI_HEARTBEAT_SECS` heartbeat and its final text both land here. The clock is
/// set at spawn, then refreshed per chunk read, so a silent-but-alive child and
/// a hung one are finally distinguishable.
#[derive(Clone, Default)]
struct Activity {
    last: Arc<Mutex<Option<Instant>>>,
    out_bytes: Arc<AtomicUsize>,
    err_bytes: Arc<AtomicUsize>,
}

impl Activity {
    /// Start the clock: a freshly spawned child counts as active.
    fn start(&self) {
        if let Ok(mut last) = self.last.lock() {
            *last = Some(Instant::now());
        }
    }

    /// Record `bytes` arriving on `stream` — resets the silence window.
    fn touch(&self, stream: Stream, bytes: usize) {
        let counter = match stream {
            Stream::Out => &self.out_bytes,
            Stream::Err => &self.err_bytes,
        };
        counter.fetch_add(bytes, Ordering::Relaxed);
        if let Ok(mut last) = self.last.lock() {
            *last = Some(Instant::now());
        }
    }

    /// How long the child has been silent (stdout + stderr).
    fn silence(&self) -> Duration {
        self.last
            .lock()
            .ok()
            .and_then(|last| *last)
            .map(|last| last.elapsed())
            .unwrap_or_default()
    }

    /// `(stdout bytes, stderr bytes)` read so far.
    fn bytes(&self) -> (usize, usize) {
        (
            self.out_bytes.load(Ordering::Relaxed),
            self.err_bytes.load(Ordering::Relaxed),
        )
    }
}

/// Build the child command. Separate from `run` so it is unit-testable without
/// spawning anything.
///
/// `provider` / `model` are passed through as `--provider` / `--model`. The child
/// shares the orchestrator's agent dir, so without them it silently falls back to
/// the *default* provider — which is wrong (or broken) whenever the orchestrator
/// itself was started with an explicit `--provider` / `--model`.
pub fn child_command(
    exe: &Path,
    max_turns: u64,
    provider: Option<&str>,
    model: Option<&str>,
) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("-p")
        // Ephemeral session: a one-shot worker has nothing to resume, and its
        // session file would otherwise be written into the parent's project
        // `.ki/sessions/` directory — one ~2 MB file per delegation.
        .arg("--no-session")
        // Recursion guard: the child must not offer `subagent`/`plan`/`dispatch`.
        .arg("--exclude-tools")
        .arg(EXCLUDED_TOOLS);
    if let Some(provider) = provider.filter(|value| !value.is_empty()) {
        command.arg("--provider").arg(provider);
    }
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        command.arg("--model").arg(model);
    }
    command
        // Hard turn ceiling enforced by the harness, not the model.
        .env(MAX_TURNS_ENV, max_turns.to_string())
        .env(SUBAGENT_ENV, "1")
        // Child-side liveness heartbeat: without it a long tool call looks
        // exactly like a hang (print mode's stdout is silent until the end).
        .env(HEARTBEAT_SECS_ENV, CHILD_HEARTBEAT_SECS.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group, so cancelling can kill the child *and its descendants*
    // instead of orphaning whatever it spawned.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

/// Truncate to [`MAX_OUTPUT_BYTES`] on a char boundary, appending a marker.
pub fn truncate_output(mut text: String) -> String {
    const MARKER: &str = "\n[subagent output truncated]";
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(MARKER);
    text
}

/// Run one delegation using the current executable as the child `ki`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    task: &str,
    max_turns: u64,
    timeout_secs: u64,
    provider: Option<&str>,
    model: Option<&str>,
    control: &Control,
) -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot resolve the ki executable: {err}"))?;
    run_with_exe(
        &exe,
        task,
        max_turns,
        timeout_secs,
        provider,
        model,
        control,
    )
}

/// Run one delegation with an explicit executable (test seam).
#[allow(clippy::too_many_arguments)]
pub fn run_with_exe(
    exe: &Path,
    task: &str,
    max_turns: u64,
    timeout_secs: u64,
    provider: Option<&str>,
    model: Option<&str>,
    control: &Control,
) -> Result<String, String> {
    // Fail before launching anything if already cancelled.
    control.check()?;
    let activity = Activity::default();
    let mut child = child_command(exe, max_turns, provider, model)
        .spawn()
        .map_err(|err| format!("failed to start subagent `{}`: {err}", exe.display()))?;
    activity.start();
    stage(&format!("spawn pid={}", child.id()));

    // Drain stdout/stderr on their own threads so a chatty child cannot block on
    // a full pipe while we wait for it to exit. Both readers also refresh the
    // activity clock — a byte on either pipe means the child is alive.
    let out_reader = child
        .stdout
        .take()
        .map(|stdout| spawn_reader(stdout, activity.clone(), Stream::Out));
    let err_reader = child
        .stderr
        .take()
        .map(|stderr| spawn_reader(stderr, activity.clone(), Stream::Err));

    write_stdin(&mut child, task);
    stage("stdin-written");

    // Wait in slices so cancellation and the wall-clock deadline are both observed
    // while the child is still running.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs);
    let mut next_stage = started + STAGE_LOG_EVERY;
    stage("wait");
    loop {
        if control.is_cancelled() {
            kill_and_reap(&mut child);
            return Err("subagent cancelled".into());
        }
        if Instant::now() >= deadline {
            kill_and_reap(&mut child);
            return Err(format!("subagent timed out after {timeout_secs}s"));
        }
        // Periodic progress snapshot (opt-in stage log only; no behaviour change).
        if Instant::now() >= next_stage {
            let (out_bytes, err_bytes) = activity.bytes();
            stage(&format!(
                "wait elapsed={}s out={out_bytes}B err={err_bytes}B silent={}ms",
                started.elapsed().as_secs(),
                activity.silence().as_millis()
            ));
            next_stage = Instant::now() + STAGE_LOG_EVERY;
        }
        let slice = deadline
            .saturating_duration_since(Instant::now())
            .min(POLL_SLICE);
        match child.wait_timeout(slice) {
            Ok(Some(_)) => break,
            Ok(None) => continue,
            Err(err) => {
                kill_and_reap(&mut child);
                return Err(format!("failed while waiting for subagent: {err}"));
            }
        }
    }

    // Reap the whole group on the normal-exit path too: a descendant that
    // outlived the child would hold the stdout/stderr write end, `join_reader`
    // would never see EOF, and the worker would hang here past every
    // cancellation checkpoint. Same guard the host's exec capability runs
    // after `child.wait()`; on an exited child this only collects the
    // survivors.
    kill_and_reap(&mut child);
    let status = child
        .wait()
        .map_err(|err| format!("failed to reap subagent: {err}"))?;
    stage(&format!(
        "reaped status={} success={}",
        status.code().unwrap_or(-1),
        status.success()
    ));
    let mut text = join_reader(out_reader);
    let stderr_text = join_reader(err_reader);
    stage("readers-joined");
    if !status.success() && !stderr_text.trim().is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(stderr_text.trim_end());
    }
    let text = truncate_output(text);
    if !status.success() {
        return Err(format!(
            "subagent exited with {}:\n{}",
            status.code().unwrap_or(-1),
            text
        ));
    }
    Ok(text)
}

fn kill_and_reap(child: &mut Child) {
    #[cfg(unix)]
    {
        // Descendants can re-group: ki's own `bash` tool puts each shell in a new
        // process group, so a group kill from here would miss them. Walk the tree
        // first (while it is still attached), then kill the group and the child.
        kill_descendants(child.id());
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(-(child.id() as i32)), Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Best-effort recursive SIGKILL of `pid` and everything below it (unix).
#[cfg(unix)]
fn kill_descendants(pid: u32) {
    if let Ok(output) = std::process::Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Ok(child) = line.trim().parse::<u32>() {
                kill_descendants(child);
            }
        }
    }
    let _ = std::process::Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

fn write_stdin(child: &mut Child, task: &str) {
    if let Some(mut stdin) = child.stdin.take() {
        // A child that exits before reading yields EPIPE; not fatal — the exit
        // status is what decides success.
        let _ = stdin.write_all(task.as_bytes());
        let _ = stdin.flush();
        // Dropping `stdin` closes the pipe so the child observes EOF.
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    activity: Activity,
    stream: Stream,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(bytes) => {
                    buf.extend_from_slice(&chunk[..bytes]);
                    // Every chunk is progress: this is what tells the wait loop
                    // the child is working, not hung.
                    activity.touch(stream, bytes);
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                // Same tolerance `read_to_end` had: a read error ends the stream
                // and the partial buffer still gets returned.
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Stage-log destination, resolved once from [`DEBUG_LOG_ENV`].
fn stage_log_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let value = std::env::var(DEBUG_LOG_ENV).ok()?;
        let value = value.trim();
        if value.is_empty() || value == "0" {
            return None;
        }
        if value == "1" || value.eq_ignore_ascii_case("true") {
            Some(std::env::temp_dir().join("kiqi-subagent.log"))
        } else {
            Some(PathBuf::from(value))
        }
    })
    .as_ref()
}

/// Append one timestamped stage line; a no-op unless [`DEBUG_LOG_ENV`] is set.
///
/// Diagnostics must never change a run's outcome, so every IO error (a missing
/// temp dir, a full disk, a path pointing at a directory) is ignored and nothing
/// panics.
fn stage(line: &str) {
    let Some(path) = stage_log_path() else {
        return;
    };
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "{} {line}", stage_timestamp()));
}

/// UTC timestamp, `2026-09-24T12:00:01.234Z` — the same format `key_trace` uses,
/// inlined here so the plugin keeps zero extra crate dependencies.
fn stage_timestamp() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let secs = (millis / 1000) as i64;
    let sub = millis % 1000;
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    // Days-from-civil inverse (Howard Hinnant) — no date crate needed.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{sub:03}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

fn join_reader(handle: Option<std::thread::JoinHandle<String>>) -> String {
    handle
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_command_sets_recursion_guard_and_turn_budget() {
        let command = child_command(Path::new("/usr/bin/ki"), 7, None, None);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec!["-p", "--no-session", "--exclude-tools", EXCLUDED_TOOLS]
        );

        // `Command::get_envs` iterates in an unspecified order, so sort before the
        // exact comparison (the set, not the order, is the contract).
        let mut envs: Vec<(String, String)> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        envs.sort();
        assert_eq!(
            envs,
            vec![
                (
                    HEARTBEAT_SECS_ENV.to_string(),
                    CHILD_HEARTBEAT_SECS.to_string()
                ),
                (MAX_TURNS_ENV.to_string(), "7".to_string()),
                (SUBAGENT_ENV.to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn child_command_passes_provider_and_model_through() {
        let command = child_command(
            Path::new("/usr/bin/ki"),
            8,
            Some("deepseek"),
            Some("deepseek-flash"),
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-p",
                "--no-session",
                "--exclude-tools",
                EXCLUDED_TOOLS,
                "--provider",
                "deepseek",
                "--model",
                "deepseek-flash",
            ]
        );

        // Empty strings must be dropped rather than emitted as bare flags.
        let bare = child_command(Path::new("/usr/bin/ki"), 8, Some(""), None);
        let bare_args: Vec<String> = bare
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            bare_args,
            vec!["-p", "--no-session", "--exclude-tools", EXCLUDED_TOOLS]
        );
    }

    #[test]
    fn truncate_output_is_bounded_and_char_safe() {
        let short = "ok".to_string();
        assert_eq!(truncate_output(short.clone()), short);

        // Multi-byte chars: the cut must land on a boundary, not split a char.
        let long = "あ".repeat(MAX_OUTPUT_BYTES);
        let out = truncate_output(long);
        assert!(out.starts_with('あ'));
        assert!(out.ends_with("[subagent output truncated]"));
        assert!(out.len() <= MAX_OUTPUT_BYTES + "\n[subagent output truncated]".len());
    }

    #[cfg(unix)]
    #[test]
    /// Regression: on a NORMAL exit the process group was never reaped, so a
    /// descendant that outlived the child held the stdout write end and
    /// `join_reader` hung forever, past every cancellation checkpoint. The
    /// post-exit reap collects the survivor; the call must return.
    #[cfg(unix)]
    #[test]
    fn a_grandchild_holding_the_pipe_cannot_hang_a_normal_exit() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir();
        let script = dir.join(format!("orch-pipe-{}.sh", std::process::id()));
        {
            let mut file = std::fs::File::create(&script).unwrap();
            writeln!(file, "#!/bin/sh").unwrap();
            writeln!(file, "sleep 30 &").unwrap();
            writeln!(file, "echo done").unwrap();
        }
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        let script_for_child = script.clone();
        std::thread::spawn(move || {
            let control = Control::default();
            let result = run_with_exe(&script_for_child, "ignored", 8, 20, None, None, &control);
            let _ = sender.send(result);
        });
        let text = receiver
            .recv_timeout(Duration::from_secs(15))
            .expect("a normal exit must not hang on a pipe-holding grandchild");
        assert!(text.is_ok(), "the child itself succeeded: {text:?}");
        assert!(text.unwrap().contains("done"));

        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn cancellation_kills_the_whole_process_group() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // The child backgrounds a `sleep` (a grandchild) and records its pid, then
        // waits. Cancelling must kill the group, not just the direct child.
        let dir = std::env::temp_dir();
        let script = dir.join(format!("orch-group-{}.sh", std::process::id()));
        let pidfile = dir.join(format!("orch-group-{}.pid", std::process::id()));
        {
            let mut file = std::fs::File::create(&script).unwrap();
            writeln!(file, "#!/bin/sh").unwrap();
            writeln!(file, "sleep 30 &").unwrap();
            writeln!(file, "echo $! > {}", pidfile.display()).unwrap();
            writeln!(file, "wait").unwrap();
        }
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let control = Control::default();
        let canceller = control.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            canceller.cancel();
        });
        let _ = run_with_exe(&script, "ignored", 8, 30, None, None, &control);
        let _ = handle.join();

        let grandchild = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        std::thread::sleep(Duration::from_millis(300));
        if let Some(pid) = grandchild {
            let alive = std::process::Command::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            assert!(!alive, "grandchild {pid} survived the process-group kill");
        }
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&pidfile);
    }

    #[test]
    fn cancelled_control_aborts_the_call() {
        // A pre-cancelled control aborts before any child is launched, so this
        // holds even where the fake executable path does not exist.
        let control = Control::default();
        control.cancel();
        let err = run_with_exe(
            Path::new("/nonexistent/orchestrator-child"),
            "ignored",
            8,
            5,
            None,
            None,
            &control,
        )
        .unwrap_err();
        assert!(err.contains("cancelled"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_a_running_child() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // A stand-in child that ignores our args and blocks, so the wait loop has
        // something to kill. `exec` makes the script process become the sleeper,
        // so killing the direct child leaves no orphan.
        let script = std::env::temp_dir().join(format!("orch-cancel-{}.sh", std::process::id()));
        {
            let mut file = std::fs::File::create(&script).unwrap();
            writeln!(file, "#!/bin/sh").unwrap();
            writeln!(file, "exec sleep 30").unwrap();
        }
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let control = Control::default();
        let canceller = control.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            canceller.cancel();
        });
        let start = Instant::now();
        let err = run_with_exe(&script, "ignored", 8, 30, None, None, &control).unwrap_err();
        let elapsed = start.elapsed();
        let _ = handle.join();
        let _ = std::fs::remove_file(&script);

        assert!(err.contains("cancelled"), "got: {err}");
        assert!(
            elapsed < Duration::from_secs(10),
            "cancel took too long: {elapsed:?}"
        );
    }

    /// Reader that yields one chunk per `read`, sleeping before each, so the
    /// activity clock can be observed mid-stream.
    struct ChunkedReader {
        chunks: std::collections::VecDeque<(Duration, Vec<u8>)>,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let Some((delay, chunk)) = self.chunks.pop_front() else {
                return Ok(0);
            };
            std::thread::sleep(delay);
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            if n < chunk.len() {
                self.chunks
                    .push_front((Duration::ZERO, chunk[n..].to_vec()));
            }
            Ok(n)
        }
    }

    #[test]
    fn spawn_reader_refreshes_the_activity_clock_on_every_chunk() {
        let gap = Duration::from_millis(400);
        let first = b"one\n".to_vec();
        let second = b"two\n".to_vec();
        let reader = ChunkedReader {
            chunks: vec![
                (Duration::ZERO, first.clone()),
                (gap, second.clone()),
                (gap, b"three\n".to_vec()),
            ]
            .into(),
        };

        let activity = Activity::default();
        activity.start();
        let handle = spawn_reader(reader, activity.clone(), Stream::Err);

        // As soon as the second chunk lands the clock must have been reset. If the
        // reader only counted bytes (or only touched the clock once at EOF), the
        // silence window here would already be ~one full gap.
        let second_end = first.len() + second.len();
        let deadline = Instant::now() + Duration::from_secs(10);
        while activity.bytes().1 < second_end && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let silence = activity.silence();
        assert!(
            silence < Duration::from_millis(200),
            "the clock was not refreshed by the arriving chunk: silence={silence:?}"
        );

        let text = join_reader(Some(handle));
        assert_eq!(text, "one\ntwo\nthree\n");
        assert_eq!(
            activity.bytes(),
            (0, text.len()),
            "stderr bytes are counted on the stderr counter only"
        );
    }

    #[test]
    fn activity_clock_counts_both_pipes_and_starts_at_spawn() {
        let activity = Activity::default();
        assert_eq!(activity.bytes(), (0, 0));
        activity.start();
        assert!(activity.silence() < Duration::from_secs(1));

        activity.touch(Stream::Out, 5);
        activity.touch(Stream::Err, 7);
        assert_eq!(activity.bytes(), (5, 7));
        assert!(activity.silence() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_keeps_writing_to_stderr_still_returns_its_text() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        // The shape of a real subagent: periodic stderr output (the heartbeat)
        // and the final answer on stdout at the very end.
        let script = std::env::temp_dir().join(format!("orch-chatty-{}.sh", std::process::id()));
        {
            let mut file = std::fs::File::create(&script).unwrap();
            writeln!(file, "#!/bin/sh").unwrap();
            writeln!(file, "for i in 1 2 3 4 5; do").unwrap();
            writeln!(file, "  echo \"[ki] heartbeat t=$i\" >&2").unwrap();
            writeln!(file, "  sleep 0.2").unwrap();
            writeln!(file, "done").unwrap();
            writeln!(file, "echo done").unwrap();
        }
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let control = Control::default();
        let result = run_with_exe(&script, "ignored", 8, 30, None, None, &control);
        let _ = std::fs::remove_file(&script);

        let text = result.expect("a chatty child that exits normally is a success");
        assert!(text.contains("done"), "got: {text:?}");
    }
}
