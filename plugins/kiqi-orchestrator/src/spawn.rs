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
//!
//! # Liveness
//!
//! `ki -p` writes its final text only when the run ends, so **stdout silence is
//! not a stall signal** — a child inside a long tool call looks exactly like a
//! wedged one. Two signals make the difference observable:
//!
//! - the child heartbeats on stderr every [`CHILD_HEARTBEAT_SECS`] seconds
//!   (`KI_HEARTBEAT_SECS`, implemented in `kiqi-cli`'s print mode);
//! - both readers refresh a shared [`Activity`] clock per chunk, so *any* byte on
//!   either pipe counts as progress.
//!
//! The wait loop then only declares a stall when nothing at all arrived for
//! [`SUBAGENT_STALL_SECS`]. Ordering is cancellation → wall-clock timeout → stall,
//! so an explicit cancel never waits for a window.

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

/// How long the child may produce **nothing** on either pipe before the call is
/// declared stalled and killed.
///
/// This is not a timeout: activity is measured from the last byte read, and the
/// child heartbeats every [`CHILD_HEARTBEAT_SECS`] seconds, so a genuinely long
/// task keeps resetting the clock. The window only exists to bound the failure
/// mode where the child is wedged with no output at all — two measured hangs sat
/// for 17 and 29 minutes.
pub const SUBAGENT_STALL_SECS: u64 = 180;

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
    run_with_limits(
        exe,
        task,
        max_turns,
        timeout_secs,
        SUBAGENT_STALL_SECS,
        provider,
        model,
        control,
    )
}

/// [`run_with_exe`] with the stall window injected, so tests can exercise it in
/// seconds instead of minutes. Production always passes [`SUBAGENT_STALL_SECS`].
#[allow(clippy::too_many_arguments)]
fn run_with_limits(
    exe: &Path,
    task: &str,
    max_turns: u64,
    timeout_secs: u64,
    stall_secs: u64,
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
    let out_sink: OutputSink = Arc::new(Mutex::new(Vec::new()));
    let err_sink: OutputSink = Arc::new(Mutex::new(Vec::new()));
    let out_reader = child
        .stdout
        .take()
        .map(|stdout| spawn_reader(stdout, activity.clone(), Stream::Out, Arc::clone(&out_sink)));
    let err_reader = child
        .stderr
        .take()
        .map(|stderr| spawn_reader(stderr, activity.clone(), Stream::Err, Arc::clone(&err_sink)));

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
        // Stall: cancellation and the wall-clock deadline win above, so a wedged
        // child is only declared stalled while the call is otherwise healthy.
        // The window is measured from the last byte on *either* pipe, and the
        // child heartbeats every `CHILD_HEARTBEAT_SECS`, so a long task that is
        // still working never trips this.
        let silence = activity.silence();
        if silence > Duration::from_secs(stall_secs) {
            kill_and_reap(&mut child);
            stage(&format!("stalled silent={}ms", silence.as_millis()));
            return Err(format!("subagent stalled: no activity for {stall_secs}s"));
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
    let mut text = join_reader(out_reader, &out_sink, READER_JOIN_TIMEOUT);
    let stderr_text = join_reader(err_reader, &err_sink, READER_JOIN_TIMEOUT);
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
    // A zero exit with nothing on stdout is not a success this tool can use.
    //
    // `ki -p` writes its answer once, at the end of the run, so an empty stdout
    // means the child never produced a final message. A run stopped by the turn
    // ceiling (`KI_MAX_TURNS_PER_RUN`, which `child_command` sets from the node's
    // `maxTurns`) now exits non-zero and is caught above, so this arm is a
    // backstop for a child that succeeds yet still prints nothing.
    //
    // Returning `Err` keeps such a node visible as a failure instead of marking
    // it `done` with an empty result, with the child's stderr (where `ki` says
    // anything it can about the run) as the diagnostic.
    if text.trim().is_empty() {
        let detail = if stderr_text.trim().is_empty() {
            "(no stdout, no stderr)".to_string()
        } else {
            stderr_text.trim_end().to_string()
        };
        return Err(format!(
            "subagent exited successfully but produced no output; the run may have \
             hit its turn budget (maxTurns={max_turns}). stderr:\n{detail}"
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

/// Where a reader thread's bytes land.
///
/// The reader appends here instead of returning its buffer from the thread, so the
/// caller can collect everything read so far **even if the thread is still
/// blocked** in `read`. That is what lets `join_reader` give up waiting instead of
/// hanging on a pipe whose write end nobody will ever close.
type OutputSink = Arc<Mutex<Vec<u8>>>;

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    activity: Activity,
    stream: Stream,
    sink: OutputSink,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(bytes) => {
                    if let Ok(mut buf) = sink.lock() {
                        buf.extend_from_slice(&chunk[..bytes]);
                    }
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

/// How long to wait for a reader thread to reach EOF once the child is gone.
///
/// A reader can block forever if some process still holds the write end of the
/// pipe: the child's own group is killed on every exit path, but a descendant that
/// left the process group cannot be reached that way, and waiting for it is not an
/// option — the parent run would hang with the child already dead. This bound, not
/// the reap, is what keeps the call returnable.
///
/// Old behaviour: unbounded (`handle.join()`). New: 5 s, after which whatever was
/// read is kept and the thread is left to finish on its own.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

fn join_reader(
    handle: Option<std::thread::JoinHandle<()>>,
    sink: &OutputSink,
    timeout: Duration,
) -> String {
    if let Some(handle) = handle {
        let deadline = Instant::now() + timeout;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
        // Otherwise the thread is left detached, still blocked in `read` on a pipe
        // nobody will close. Dropping the handle is deliberate: joining it is what
        // used to hang the whole run.
    }
    let bytes = sink.lock().map(|buf| buf.clone()).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
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

    /// A reader that yields some bytes and then never reaches EOF, standing in for
    /// a pipe whose write end is held by a process the reap could not reach (a
    /// descendant that left the process group). `join()` on its thread never
    /// returns, which is what used to hang the whole run with the child already
    /// dead.
    struct StuckReader {
        released: std::sync::mpsc::Receiver<()>,
        sent: bool,
    }

    impl Read for StuckReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent {
                self.sent = true;
                let bytes = b"partial output\n";
                buf[..bytes.len()].copy_from_slice(bytes);
                return Ok(bytes.len());
            }
            // No EOF until the test releases us. The 60 s cap exists so a mutant
            // that joins without a bound fails a bounded assertion instead of
            // hanging the suite forever.
            let _ = self.released.recv_timeout(Duration::from_secs(60));
            Ok(0)
        }
    }

    /// The bound, not the reap, is what makes the call returnable: a reader stuck
    /// on a pipe nobody closes must not be joined indefinitely.
    #[test]
    fn a_reader_that_never_reaches_eof_cannot_block_the_call() {
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let sink: OutputSink = Arc::new(Mutex::new(Vec::new()));
        let activity = Activity::default();
        activity.start();
        let handle = spawn_reader(
            StuckReader {
                released: release_rx,
                sent: false,
            },
            activity,
            Stream::Out,
            Arc::clone(&sink),
        );

        let start = Instant::now();
        // A test-sized bound; production uses READER_JOIN_TIMEOUT.
        let text = join_reader(Some(handle), &sink, Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "the join must be bounded, took {elapsed:?}"
        );
        assert_eq!(
            text, "partial output\n",
            "the bytes read before the stall must survive"
        );
        // Let the helper thread finish so the test leaves nothing running.
        let _ = release_tx.send(());
    }

    #[test]
    fn a_finished_reader_still_returns_all_of_its_output() {
        let sink: OutputSink = Arc::new(Mutex::new(Vec::new()));
        let activity = Activity::default();
        activity.start();
        let handle = spawn_reader(
            std::io::Cursor::new(b"all of it\n".to_vec()),
            activity,
            Stream::Out,
            Arc::clone(&sink),
        );
        let text = join_reader(Some(handle), &sink, READER_JOIN_TIMEOUT);
        assert_eq!(text, "all of it\n");
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
        let sink: OutputSink = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_reader(reader, activity.clone(), Stream::Err, Arc::clone(&sink));

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

        let text = join_reader(Some(handle), &sink, READER_JOIN_TIMEOUT);
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

    /// Write an executable stand-in child; the name keeps concurrent tests apart.
    #[cfg(unix)]
    fn script_file(name: &str, body: &str) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!("orch-{name}-{}.sh", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "#!/bin/sh").unwrap();
        writeln!(file, "{body}").unwrap();
        drop(file);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// `kill -0` probe: succeeds for a live **or zombie** process, so a `false`
    /// here means the child was actually reaped.
    #[cfg(unix)]
    fn is_alive(pid: i32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    #[test]
    fn a_silent_child_is_killed_as_stalled() {
        // The measured failure mode: a child wedged with no output at all. It
        // records its pid and then blocks forever without writing a byte.
        let pidfile = std::env::temp_dir().join(format!("orch-stall-{}.pid", std::process::id()));
        let script = script_file(
            "stall",
            &format!("echo $$ > {}\nexec sleep 30", pidfile.display()),
        );

        let control = Control::default();
        let start = Instant::now();
        // Production passes `SUBAGENT_STALL_SECS`; a 1s window keeps the test fast.
        let err = run_with_limits(&script, "ignored", 8, 60, 1, None, None, &control).unwrap_err();
        let elapsed = start.elapsed();

        assert!(err.contains("stalled"), "got: {err}");
        assert!(
            !err.contains("timed out"),
            "a stall must not be reported as a timeout: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "the stall window was not honoured: {elapsed:?}"
        );

        let pid = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .expect("the child records its pid");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!is_alive(pid), "stalled child {pid} was left behind");

        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&pidfile);
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_keeps_heartbeating_is_not_stalled() {
        // 1s worth of silence at a time against a 2s window: exactly the shape a
        // real subagent has (`KI_HEARTBEAT_SECS=15`), just faster. The script also
        // refuses to run without the env var, so dropping it from `child_command`
        // reddens this test rather than passing silently.
        let script = script_file(
            "beat",
            "if [ -z \"$KI_HEARTBEAT_SECS\" ]; then\n  echo 'started without KI_HEARTBEAT_SECS' >&2\n  exit 3\nfi\nfor i in 1 2 3; do\n  echo \"[ki] heartbeat t=$i\" >&2\n  sleep 1\ndone\necho done",
        );

        let control = Control::default();
        let result = run_with_limits(&script, "ignored", 8, 60, 2, None, None, &control);
        let _ = std::fs::remove_file(&script);

        let text = result.expect(&format!(
            "a heartbeating child is not stalled (child must get {HEARTBEAT_SECS_ENV})"
        ));
        assert!(text.contains("done"), "got: {text:?}");
    }

    #[test]
    fn the_child_heartbeat_is_comfortably_faster_than_the_stall_window() {
        // A heartbeat at or above the window would make a busy child look stalled.
        assert!(
            CHILD_HEARTBEAT_SECS * 3 <= SUBAGENT_STALL_SECS,
            "heartbeat {CHILD_HEARTBEAT_SECS}s is too close to the {SUBAGENT_STALL_SECS}s stall window"
        );
    }

    /// A run that hits its turn ceiling still exits 0 (`derive_outcome` treats a
    /// budget stop as a normal exit), and `ki -p` then has no final message to
    /// print. Reporting that as success would tell the orchestrator `done` for
    /// work that was cut off — the node would vanish from the board as finished.
    #[cfg(unix)]
    #[test]
    fn a_successful_exit_with_no_output_is_a_failure() {
        // Exits 0, writes only to stderr, exactly like a truncated run.
        let script = script_file(
            "silent-success",
            "echo 'Stopped after 2 turns in a single run (limit 2)' >&2\nexit 0",
        );

        let control = Control::default();
        let result = run_with_limits(&script, "ignored", 8, 60, 30, None, None, &control);
        let _ = std::fs::remove_file(&script);

        let err = result.expect_err("no output must not be reported as a success");
        assert!(
            err.contains("produced no output"),
            "the error has to name the problem: {err}"
        );
        assert!(
            err.contains("turn budget"),
            "the error must point at the likely cause: {err}"
        );
        assert!(
            err.contains("Stopped after 2 turns"),
            "the child's own stderr is the only diagnostic we have: {err}"
        );
    }

    /// The counterpart: white-space-only stdout is still nothing. A child that
    /// prints a newline and exits must not pass as delivered work either.
    #[cfg(unix)]
    #[test]
    fn white_space_output_alone_does_not_count_as_a_result() {
        let script = script_file("blank-success", "printf '\\n\\n'\nexit 0");

        let control = Control::default();
        let result = run_with_limits(&script, "ignored", 8, 60, 30, None, None, &control);
        let _ = std::fs::remove_file(&script);

        let err = result.expect_err("blank output is not a result");
        assert!(err.contains("produced no output"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_child_that_exits_immediately_returns_and_leaves_no_zombie() {
        let pidfile = std::env::temp_dir().join(format!("orch-quick-{}.pid", std::process::id()));
        let script = script_file(
            "quick",
            &format!("echo $$ > {}\necho done", pidfile.display()),
        );

        let control = Control::default();
        let start = Instant::now();
        let result = run_with_limits(&script, "ignored", 8, 60, 1, None, None, &control);
        let elapsed = start.elapsed();

        let text = result.expect("an immediately exiting child is a success");
        assert!(text.contains("done"), "got: {text:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "an immediate exit must not wait for the stall window: {elapsed:?}"
        );

        let pid = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .expect("the child records its pid");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!is_alive(pid), "child {pid} was left behind");

        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&pidfile);
    }
}
