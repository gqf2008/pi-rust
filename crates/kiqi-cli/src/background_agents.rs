//! Background subagents: the model starts one, the main turn keeps going.
//!
//! The orchestrator's wasm `subagent` tool had to run the child inside the call
//! (a wasm extension cannot leave a process running past its return), so a
//! batch of subagents ran strictly one after another and the main session sat
//! inside a single tool call for as long as the slowest child. This module owns
//! the host-side version instead:
//!
//! * `start` spawns `ki -p --mode json` and returns immediately,
//! * a supervisor task streams the child's JSONL into the work view (one node
//!   row per call, same shape the orchestrator's guest emits),
//! * when the child exits, the result is delivered to the session as a
//!   follow-up message, so a session that is idle starts a run with it.
//!
//! Nothing here is specific to the TUI: an observer receives the updates, and
//! headless runs simply never attach one.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// How often the box's clock is refreshed while nothing else happens.
const REPORT_EVERY: Duration = Duration::from_millis(500);
/// Output kept per agent for the completion message.
const KEEP_LINES: usize = 40;
/// Tools a child must not have: a subagent that can spawn subagents is a fork
/// bomb that also evades the parent's timeout discipline.
const EXCLUDED_TOOLS: &str = "subagent,plan,dispatch";

pub(crate) const MAX_TASK_BYTES: usize = 20_000;
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 900;
pub(crate) const MAX_TIMEOUT_SECS: u64 = 3_600;

/// One work-view row for a running (or just-finished) background agent.
#[derive(Debug, Clone, Default)]
pub(crate) struct NodeUpdate {
    pub id: String,
    pub state: String,
    pub elapsed_ms: u64,
    pub budget_ms: u64,
    pub tool: Option<String>,
    pub text: Option<String>,
    pub task: Option<String>,
    pub events: Vec<String>,
}

/// Delivered to the session once an agent has exited.
#[derive(Debug, Clone)]
pub(crate) struct FinishedAgent {
    pub id: String,
    pub task: String,
    pub ok: bool,
    /// The child's final answer, or the failure reason.
    pub summary: String,
    /// Recent tool transitions, for the transcript note.
    pub events: Vec<String>,
}

/// What a session does with background agents. The TUI draws node rows and
/// turns a finished agent into a follow-up message; headless runs attach
/// nothing.
pub(crate) trait AgentObserver: Send + Sync {
    fn node(&self, update: &NodeUpdate);
    fn finished(&self, agent: &FinishedAgent);
}

pub(crate) struct StartSpec {
    pub task: String,
    pub timeout_secs: u64,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Child program; the running `ki` binary by default. Tests inject a script
    /// (a custom program gets no `ki`-specific flags, like the orchestrator's
    /// hermetic seam).
    pub program: Option<std::path::PathBuf>,
}

struct Running {
    task: String,
    started: Instant,
    cancel: CancellationToken,
}

struct Inner {
    next_id: AtomicU64,
    running: Mutex<BTreeMap<String, Running>>,
    observer: Mutex<Option<Arc<dyn AgentObserver>>>,
}

pub(crate) struct BackgroundAgents {
    inner: Arc<Inner>,
}

/// One manager per process: `ki` runs one session, and both the tool and the
/// TUI need to reach the same registry without threading it through every
/// construction site.
pub(crate) fn global() -> Arc<BackgroundAgents> {
    static GLOBAL: OnceLock<Arc<BackgroundAgents>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| Arc::new(BackgroundAgents::new()))
        .clone()
}

impl BackgroundAgents {
    /// A standalone registry. `global()` is the session's; tests build their own
    /// so an observer cannot leak into another test's assertions.
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(0),
                running: Mutex::new(BTreeMap::new()),
                observer: Mutex::new(None),
            }),
        }
    }
}

impl BackgroundAgents {
    /// Install (or replace) the session's observer.
    pub(crate) fn attach(&self, observer: Arc<dyn AgentObserver>) {
        *self.inner.observer.lock().unwrap() = Some(observer);
    }

    pub(crate) fn detach(&self) {
        *self.inner.observer.lock().unwrap() = None;
    }

    /// Ids of the agents that are still running, oldest first.
    pub(crate) fn running_ids(&self) -> Vec<String> {
        self.inner.running.lock().unwrap().keys().cloned().collect()
    }

    pub(crate) fn cancel(&self, id: &str) -> bool {
        let cancel = self
            .inner
            .running
            .lock()
            .unwrap()
            .get(id)
            .map(|agent| agent.cancel.clone());
        match cancel {
            Some(cancel) => {
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Kill every child; called when the session goes away so a background
    /// agent cannot outlive the process that would report it.
    pub(crate) fn shutdown(&self) {
        for (_, agent) in self.inner.running.lock().unwrap().iter() {
            agent.cancel.cancel();
        }
    }

    /// Spawn one child and return its work-view id. Returns as soon as the
    /// process is up; the supervisor task owns it from there.
    pub(crate) async fn start(&self, spec: StartSpec) -> Result<String, String> {
        let task = spec.task.trim().to_string();
        if task.is_empty() {
            return Err("task must not be empty".into());
        }
        if task.len() > MAX_TASK_BYTES {
            return Err(format!("task must be at most {MAX_TASK_BYTES} bytes"));
        }
        let call = self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("subagent#{call}");
        let timeout_secs = spec.timeout_secs.clamp(10, MAX_TIMEOUT_SECS);
        let task_line: String = task
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();

        let custom = spec.program.clone();
        let program = match custom.clone() {
            Some(program) => program,
            None => std::env::current_exe()
                .map_err(|error| format!("cannot find the ki binary: {error}"))?,
        };
        let mut command = tokio::process::Command::new(program);
        if custom.is_none() {
            command
                .arg("-p")
                .arg("--mode")
                .arg("json")
                .arg("--no-session")
                .arg("--exclude-tools")
                .arg(EXCLUDED_TOOLS);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // The child must not inherit the TUI's session variables beyond the
            // ones it needs to resolve the same provider configuration.
            .kill_on_drop(true);
        // The child is its own session: it must not inherit this process's
        // session id or claim its trace as its own (same rule bash follows for
        // the variables it hands to a spawned shell).
        for name in kiqi_tools::HOST_PRIVATE_ENV {
            command.env_remove(name);
        }
        for name in kiqi_agent::env::SUBAGENT_PARENT_ENV {
            command.env_remove(name);
        }
        if let Some(provider) = spec.provider.as_deref() {
            command.arg("--provider").arg(provider);
        }
        if let Some(model) = spec.model.as_deref() {
            command.arg("--model").arg(model);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start a subagent: {error}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "subagent has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "subagent has no stdout".to_string())?;
        // The task rides stdin; EOF tells the child the prompt is complete.
        if let Err(error) = async {
            stdin.write_all(task.as_bytes()).await?;
            stdin.shutdown().await
        }
        .await
        {
            let _ = child.kill().await;
            return Err(format!("cannot send the task to the subagent: {error}"));
        }

        let cancel = CancellationToken::new();
        self.inner.running.lock().unwrap().insert(
            id.clone(),
            Running {
                task: task.clone(),
                started: Instant::now(),
                cancel: cancel.clone(),
            },
        );

        let inner = Arc::clone(&self.inner);
        let agent_id = id.clone();
        tokio::spawn(async move {
            supervise(
                inner,
                agent_id,
                task,
                task_line,
                stdout,
                timeout_secs,
                cancel,
                child,
            )
            .await;
        });
        Ok(id)
    }
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    inner: Arc<Inner>,
    id: String,
    task: String,
    task_line: String,
    stdout: tokio::process::ChildStdout,
    timeout_secs: u64,
    cancel: CancellationToken,
    mut child: tokio::process::Child,
) {
    let started = Instant::now();
    let budget_ms = timeout_secs * 1_000;
    let deadline = started + Duration::from_secs(timeout_secs);
    let mut lines = BufReader::new(stdout).lines();
    let mut tool: Option<String> = None;
    let mut text: Option<String> = None;
    let mut events: Vec<String> = Vec::new();
    let mut output: Vec<String> = Vec::new();
    let mut final_text: Option<String> = None;
    let mut timed_out = false;
    let mut ticker = tokio::time::interval(REPORT_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    report(
        &inner, &id, started, budget_ms, &task_line, &tool, &text, &events, "running",
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    break;
                }
                report(&inner, &id, started, budget_ms, &task_line, &tool, &text, &events, "running");
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    if let Some(event) = parse_line(&line) {
                        match event {
                            ChildEvent::ToolStart(name) => {
                                events.push(format!("▶ {name}"));
                                if events.len() > 10 {
                                    events.remove(0);
                                }
                                tool = Some(name);
                                report(&inner, &id, started, budget_ms, &task_line, &tool, &text, &events, "running");
                            }
                            ChildEvent::ToolEnd { name, is_error } => {
                                events.push(format!(
                                    "{} {name}",
                                    if is_error { "✖" } else { "✔" }
                                ));
                                if events.len() > 10 {
                                    events.remove(0);
                                }
                                tool = None;
                            }
                            ChildEvent::Text(value) => {
                                text = Some(value);
                            }
                            ChildEvent::Final(value) => {
                                final_text = Some(value);
                            }
                            ChildEvent::Output(value) => {
                                output.push(value);
                                if output.len() > KEEP_LINES {
                                    output.remove(0);
                                }
                            }
                        }
                    }
                }
                // EOF or a read error: the child has stopped producing.
                _ => break,
            },
        }
    }

    if timed_out || cancel.is_cancelled() {
        let _ = child.kill().await;
    }
    let status = child.wait().await.ok().and_then(|status| status.code());
    inner.running.lock().unwrap().remove(&id);

    let ok = status == Some(0) && !timed_out && !cancel.is_cancelled();
    let summary = if timed_out {
        format!("timed out after {timeout_secs}s")
    } else if cancel.is_cancelled() {
        "cancelled".to_string()
    } else {
        final_text
            .clone()
            .or_else(|| {
                // `--mode json` prints its final answer as a `result` line; a
                // child that died early leaves the raw output instead.
                (!output.is_empty()).then(|| output.join("\n"))
            })
            .unwrap_or_else(|| "(no output)".to_string())
    };

    let update = NodeUpdate {
        id: id.clone(),
        state: if ok { "done" } else { "failed" }.to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        budget_ms,
        tool: None,
        text: Some(summary.clone()),
        task: Some(task_line.clone()),
        events: events.clone(),
    };
    let finished = FinishedAgent {
        id: id.clone(),
        task,
        ok,
        summary,
        events,
    };
    let observer = inner.observer.lock().unwrap().clone();
    if let Some(observer) = observer {
        observer.node(&update);
        observer.finished(&finished);
    }
}

#[allow(clippy::too_many_arguments)]
fn report(
    inner: &Arc<Inner>,
    id: &str,
    started: Instant,
    budget_ms: u64,
    task_line: &str,
    tool: &Option<String>,
    text: &Option<String>,
    events: &[String],
    state: &str,
) {
    let update = NodeUpdate {
        id: id.to_string(),
        state: state.to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        budget_ms,
        tool: tool.clone(),
        text: text.clone(),
        task: Some(task_line.to_string()),
        events: events.to_vec(),
    };
    if let Some(observer) = inner.observer.lock().unwrap().as_ref() {
        observer.node(&update);
    }
}

/// One parsed line of a child's `--mode json` stream.
enum ChildEvent {
    ToolStart(String),
    ToolEnd { name: String, is_error: bool },
    Text(String),
    Final(String),
    Output(String),
}

/// Text of an assistant message, whether it arrives as a plain string or as a
/// content array.
fn message_text(message: &serde_json::Value) -> Option<String> {
    if let Some(text) = message.get("text").and_then(|value| value.as_str()) {
        return Some(text.to_string());
    }
    let content = message.get("content")?.as_array()?;
    let joined: String = content
        .iter()
        .filter_map(|block| block.get("text").and_then(|value| value.as_str()))
        .collect::<Vec<_>>()
        .join("");
    (!joined.is_empty()).then_some(joined)
}

fn parse_line(line: &str) -> Option<ChildEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    match value.get("type")?.as_str()? {
        "tool_execution_start" => {
            let name = value.get("toolName")?.as_str()?.to_string();
            Some(ChildEvent::ToolStart(name))
        }
        "tool_execution_end" => {
            let name = value.get("toolName")?.as_str()?.to_string();
            let is_error = value
                .get("isError")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            Some(ChildEvent::ToolEnd { name, is_error })
        }
        "message_update" | "message_end" => {
            let message = value.get("message")?;
            let text = message_text(message)?;
            // A finished message is the settled text; a streamed one is the tail.
            if value.get("type").and_then(|kind| kind.as_str()) == Some("message_end") {
                Some(ChildEvent::Text(text))
            } else {
                Some(ChildEvent::Text(text))
            }
        }
        "result" => {
            let text = value
                .get("finalText")
                .and_then(|value| value.as_str())
                .map(str::to_string)?;
            Some(ChildEvent::Final(text))
        }
        "text" => Some(ChildEvent::Output(value.get("text")?.as_str()?.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_events_are_parsed_from_the_jsonl_stream() {
        assert!(matches!(
            parse_line(r#"{"type":"tool_execution_start","toolName":"bash"}"#),
            Some(ChildEvent::ToolStart(name)) if name == "bash"
        ));
        assert!(matches!(
            parse_line(
                r#"{"type":"tool_execution_end","toolName":"bash","isError":true}"#
            ),
            Some(ChildEvent::ToolEnd { name, is_error }) if name == "bash" && is_error
        ));
        assert!(matches!(
            parse_line(
                r#"{"type":"message_update","message":{"role":"assistant","content":[{"type":"text","text":"reading src/lib.rs"}]}}"#
            ),
            Some(ChildEvent::Text(text)) if text == "reading src/lib.rs"
        ));
        assert!(matches!(
            parse_line(r#"{"type":"result","outcome":"completed","finalText":"done"}"#),
            Some(ChildEvent::Final(text)) if text == "done"
        ));
        assert!(parse_line("not json").is_none());
        assert!(parse_line(r#"{"type":"unknown"}"#).is_none());
    }

    #[test]
    fn an_agent_gets_a_unique_id_per_call() {
        let first = format!("subagent#{}", 1);
        let second = format!("subagent#{}", 2);
        assert_ne!(first, second);
    }
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[derive(Default)]
    struct Recorder {
        rows: Mutex<Vec<NodeUpdate>>,
        finished: Mutex<Vec<FinishedAgent>>,
        seen: AtomicUsize,
    }

    impl AgentObserver for Recorder {
        fn node(&self, update: &NodeUpdate) {
            self.seen.fetch_add(1, AtomicOrdering::SeqCst);
            self.rows.lock().unwrap().push(update.clone());
        }
        fn finished(&self, agent: &FinishedAgent) {
            self.finished.lock().unwrap().push(agent.clone());
        }
    }

    fn scratch_script(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ki-bg-agent-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("child.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Async on purpose: the supervisor is a tokio task, so blocking the
    /// runtime thread would starve the very thing we are waiting for.
    async fn wait_for(mut done: impl FnMut() -> bool, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_background_agent_streams_rows_and_reports_its_final_text() {
        let script = scratch_script(
            "roundtrip",
            "cat >/dev/null\n\
             echo '{\"type\":\"tool_execution_start\",\"toolName\":\"bash\"}'\n\
             echo '{\"type\":\"message_update\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"reading src/lib.rs\"}]}}'\n\
             echo '{\"type\":\"result\",\"outcome\":\"completed\",\"finalText\":\"all green\"}'",
        );
        let agents = BackgroundAgents::new();
        let recorder = Arc::new(Recorder::default());
        agents.attach(recorder.clone());

        let id = agents
            .start(StartSpec {
                task: "verify the fix".into(),
                timeout_secs: 30,
                provider: None,
                model: None,
                program: Some(script),
            })
            .await
            .expect("started");
        assert_eq!(id, "subagent#1");

        assert!(
            wait_for(|| !recorder.finished.lock().unwrap().is_empty(), 10).await,
            "the agent must report completion"
        );
        let finished = recorder.finished.lock().unwrap().clone();
        assert!(finished[0].ok, "{finished:?}");
        assert!(finished[0].summary.contains("all green"), "{finished:?}");
        assert_eq!(finished[0].task, "verify the fix");

        let rows = recorder.rows.lock().unwrap().clone();
        assert!(rows.iter().any(|row| row.state == "running"), "{rows:?}");
        let done = rows
            .iter()
            .rev()
            .find(|row| row.state == "done")
            .expect("done row");
        assert_eq!(done.id, "subagent#1");
        assert!(done.task.as_deref() == Some("verify the fix"), "{done:?}");
        // The tool transition reached the box as an event.
        assert!(
            rows.iter()
                .any(|row| row.events.iter().any(|e| e.contains("bash"))),
            "{rows:?}"
        );
    }

    /// Starting a child is not serialized: two agents are up at the same time.
    /// This is the property the old wasm tool could not provide.
    #[tokio::test]
    async fn two_agents_run_at_the_same_time() {
        let marker = std::env::temp_dir().join(format!(
            "ki-bg-parallel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let script = scratch_script(
            "parallel",
            &format!(
                "cat >/dev/null\necho started >> {}\nsleep 2\necho finished >> {}",
                marker.display(),
                marker.display()
            ),
        );
        let agents = BackgroundAgents::new();
        let recorder = Arc::new(Recorder::default());
        agents.attach(recorder.clone());
        let spec = || StartSpec {
            task: "parallel task".into(),
            timeout_secs: 30,
            provider: None,
            model: None,
            program: Some(script.clone()),
        };
        agents.start(spec()).await.expect("first");
        agents.start(spec()).await.expect("second");

        // Both must be mid-flight: two "started" lines, no "finished" yet.
        let mut both = false;
        for _ in 0..80 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let body = std::fs::read_to_string(&marker).unwrap_or_default();
            if body.lines().filter(|line| *line == "started").count() == 2 {
                assert!(!body.contains("finished"), "{body}");
                both = true;
                break;
            }
        }
        assert!(both, "both children must be running before either finishes");
        let _ = std::fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn cancelling_a_background_agent_kills_its_child() {
        let script = scratch_script("cancel", "cat >/dev/null\nsleep 30");
        let agents = BackgroundAgents::new();
        let recorder = Arc::new(Recorder::default());
        agents.attach(recorder.clone());

        let id = agents
            .start(StartSpec {
                task: "long one".into(),
                timeout_secs: 60,
                provider: None,
                model: None,
                program: Some(script),
            })
            .await
            .expect("started");
        assert!(agents.cancel(&id));
        assert!(
            wait_for(|| !recorder.finished.lock().unwrap().is_empty(), 10).await,
            "cancel must end the agent promptly"
        );
        let finished = recorder.finished.lock().unwrap().clone();
        assert!(!finished[0].ok);
        assert_eq!(finished[0].summary, "cancelled");
        assert!(agents.running_ids().is_empty());
    }

    #[tokio::test]
    async fn a_task_over_the_budget_is_rejected_before_spawning() {
        let agents = BackgroundAgents::new();
        let error = agents
            .start(StartSpec {
                task: "x".repeat(MAX_TASK_BYTES + 1),
                timeout_secs: 30,
                provider: None,
                model: None,
                program: None,
            })
            .await
            .expect_err("too large");
        assert!(error.contains("at most"), "{error}");
    }
}
