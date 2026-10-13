//! Output modes. Mirrors the v1-relevant slice of the TS
//! `packages/coding-agent/src/modes/{print-mode,json-event,rpc-mode}.ts` — the
//! three run shapes a harness-backed CLI needs:
//!
//! - [`print`] — single-shot: send the prompt(s), print the final assistant
//!   text (or the error) to stdout, exit. Mirrors TS `runPrintMode` (text mode).
//! - [`json`] — single-shot streaming: emit each harness event as a JSON line
//!   on stdout, then the final outcome. Mirrors TS `runPrintMode`
//!   (`mode === "json"`) + [`json_event::toJsonEvent`].
//! - [`interactive`] — a minimal line-oriented REPL: read prompts from stdin,
//!   run each, print the assistant text, loop until EOF / `/exit`. v1 does NOT
//!   port the TS `InteractiveMode` TUI (`modes/interactive/*` — a full terminal
//!   UI with Ink/React components); this is a deliberately minimal replacement,
//!   documented in the initial port notes (retired).
//!
//! All three drive the same `AgentHarness` via `AgentLane::prompt_text`.

use std::io::{BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex};

use rpi_agent::events::AgentEvent;
use rpi_ai::types::{AssistantMessage, Content, ImageContent, StopReason};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::events::{HarnessEvent, RunEndOutcome};

use crate::args::Args;
use crate::remote::protocol::RemoteResponse;

/// Extract the concatenated text content from an assistant message. Mirrors the
/// TS print-mode loop (`for content of assistantMsg.content if type===text`).
pub fn assistant_text(msg: &AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect()
}

/// The exit code a run's outcome maps to. Mirrors TS print mode: error/aborted
/// ⇒ exit 1; everything else ⇒ 0.
pub fn outcome_exit_code(outcome: &HarnessRunOutcome) -> i32 {
    match outcome {
        HarnessRunOutcome::Failed { .. } | HarnessRunOutcome::Aborted { .. } => 1,
        _ => 0,
    }
}

/// `print` mode: send the initial message (prompt text + inline `@file`
/// expansions), then any follow-up messages, print the final assistant text,
/// return the exit code. Mirrors TS `runPrintMode` (text).
pub async fn print(
    harness: &AgentHarness,
    _args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");

    let mut last_exit = 0;
    let mut last_msg: Option<AssistantMessage> = None;

    // The initial prompt (and its `@file` attachments) go in one user message;
    // extra positionals are separate prompts (mirrors the TS loop).
    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }

    if prompts.is_empty() {
        // Nothing to do — print mode with no prompt is a no-op success.
        return 0;
    }

    let mut images = initial_images;
    for prompt in prompts {
        match lane.prompt_text(&prompt, std::mem::take(&mut images)).await {
            Ok(result) => {
                last_exit = outcome_exit_code(&result.outcome);
                match &result.outcome {
                    HarnessRunOutcome::Completed { final_message, .. }
                    | HarnessRunOutcome::Aborted { final_message, .. } => {
                        last_msg = Some(final_message.clone());
                    }
                    HarnessRunOutcome::Failed {
                        error,
                        final_message,
                        ..
                    } => {
                        if let Some(m) = final_message {
                            if m.stop_reason == StopReason::Error {
                                if let Some(em) = &m.error_message {
                                    eprintln!("{em}");
                                }
                            }
                        }
                        eprintln!("run failed: {error:?}");
                    }
                    HarnessRunOutcome::Suspended { .. } => {
                        eprintln!("run suspended (deferred) — resume is not supported in v1");
                        last_exit = 1;
                    }
                }
            }
            Err(e) => {
                eprintln!("prompt rejected: {e}");
                return 1;
            }
        }
    }

    // Print the final assistant text to stdout (TS: writeRawStdout text + "\n").
    if let Some(m) = &last_msg {
        match m.stop_reason {
            StopReason::Error => {
                if let Some(em) = &m.error_message {
                    eprintln!("{em}");
                }
                last_exit = 1;
            }
            StopReason::Aborted => {
                eprintln!("request aborted");
                last_exit = 1;
            }
            _ => {
                let text = assistant_text(m);
                let mut out = std::io::stdout();
                let _ = out.write_all(text.as_bytes());
                if !text.ends_with('\n') {
                    let _ = out.write_all(b"\n");
                }
                let _ = out.flush();
            }
        }
    }

    last_exit
}

/// `json` mode: emit each harness event as a JSON line on stdout, run the
/// prompts, then emit a terminal `result` line carrying the outcome + final
/// text. Mirrors TS `runPrintMode` (`mode === "json"`) streaming every event.
pub async fn json(
    harness: &AgentHarness,
    _args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
    mut agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
) -> i32 {
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let collected: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_watch = collected.clone();

    // A watch captures every event (RunStart fires inline during prompt_text,
    // before a post-call listener could attach — same reason as the M5g test).
    let mut watch = harness.events().watch(|| ());
    watch.start(Arc::new(move |event: &HarnessEvent| {
        // Emit each event live as JSON, and also buffer for the final summary.
        emit_json_event(event);
        collected_for_watch.lock().unwrap().push(event.clone());
    }));
    // Keep the watch alive for the whole run. Leaking is acceptable for a
    // single-shot CLI process (the bus outlives this scope anyway).
    std::mem::forget(watch);

    // The harness bus carries run lifecycle events; the agent receiver carries
    // the native fine-grained stream (turns, message deltas, and tools).
    let (agent_done_tx, mut agent_done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let agent_event_task = agent_events.take().map(|mut rx| {
        tokio::spawn(async move {
            let done_tx = agent_done_tx;
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let terminal = event.is_terminal();
                        emit_agent_event(&event);
                        if terminal {
                            let _ = done_tx.send(());
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    });

    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }

    let mut last_exit = 0;
    let mut final_outcome: Option<HarnessRunOutcome> = None;

    let mut images = initial_images;
    for prompt in prompts {
        match lane.prompt_text(&prompt, std::mem::take(&mut images)).await {
            Ok(result) => {
                // The harness resolves after its run outcome, while the
                // broadcast listener may still be scheduling the terminal
                // AgentEnd line. Wait briefly so JSON consumers see the full
                // lifecycle before the final result summary. The timeout is
                // deliberately bounded for custom/older harness emitters.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    agent_done_rx.recv(),
                )
                .await;
                last_exit = outcome_exit_code(&result.outcome);
                final_outcome = Some(result.outcome);
            }
            Err(e) => {
                // Emit a structured error line + exit.
                let line = serde_json::json!({
                    "type": "error",
                    "error": e.to_string(),
                });
                println!("{line}");
                if let Some(task) = agent_event_task {
                    task.abort();
                }
                return 1;
            }
        }
    }

    // Terminal result summary.
    let (outcome_str, final_text) = match final_outcome {
        Some(HarnessRunOutcome::Completed { final_message, .. }) => {
            ("completed", Some(assistant_text(&final_message)))
        }
        Some(HarnessRunOutcome::Aborted { final_message, .. }) => {
            ("aborted", Some(assistant_text(&final_message)))
        }
        Some(HarnessRunOutcome::Failed { final_message, .. }) => {
            let t = final_message.as_ref().map(assistant_text);
            ("failed", t)
        }
        Some(HarnessRunOutcome::Suspended { .. }) => ("suspended", None),
        None => ("idle", None),
    };
    let result_line = serde_json::json!({
        "type": "result",
        "outcome": outcome_str,
        "finalText": final_text,
    });
    println!("{result_line}");
    if let Some(task) = agent_event_task {
        task.abort();
    }
    last_exit
}

fn emit_agent_event(event: &AgentEvent) {
    println!("{}", agent_event_json(event));
}

/// `rpc` mode: a headless JSONL command loop (the server-side agent).
///
/// Reads one JSON command per line from stdin (`{id?, type, ...}`), drives the
/// `main` lane, and writes one JSON object per line to stdout:
/// - fine-grained `AgentEvent`s via [`agent_event_json`] (each carries `type`),
/// - a `{"type":"response","id":<id>,"status":"ok"|"error",...}` per command.
///
/// Commands: `prompt` (streams the run), `abort`, `get_state`, `set_model`,
/// `set_thinking_level`, `set_active_tools`, `ping`, `stop`. `stop` (or stdin
/// EOF) ends the loop. This is the process `rpi-server` spawns and that
/// `ki --connect` talks to. Mirrors TS `runRpcMode` / `modes/rpc`.
///
/// The extension `UiDialog` mailbox is bridged over the same channel: the host
/// emits `{"type":"ui_dialog_request","requestId","toolCallId","ui"}`, and the
/// client answers with `{"type":"ui_dialog_response","requestId","answer"}` (or
/// `"cancelled":true`). A `prompt` runs in its own task so this loop keeps
/// reading — that is what lets `abort` and dialog answers arrive mid-run.
pub async fn rpc(
    harness: &AgentHarness,
    mut agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
    ui_dialog: rpi_extensions::UiDialogMailbox,
) -> i32 {
    rpc_with_io(
        harness,
        agent_events.take(),
        model_catalog,
        ui_dialog,
        tokio::io::BufReader::new(tokio::io::stdin()),
        std::io::stdout(),
    )
    .await
}

async fn rpc_with_io<R, W>(
    harness: &AgentHarness,
    mut agent_events: Option<tokio::sync::broadcast::Receiver<AgentEvent>>,
    model_catalog: Vec<rpi_ai::Model>,
    ui_dialog: rpi_extensions::UiDialogMailbox,
    input: R,
    output: W,
) -> i32
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: Write + Send + 'static,
{
    use tokio::io::AsyncBufReadExt;

    let lane: Arc<dyn AgentLane> = harness.lane("main");
    // One blocking writer owns stdout. Producers only enqueue bounded messages,
    // so pipe backpressure never blocks a Tokio worker on IO or a std mutex.
    let (stdout, writer_task, writer_failed) = start_rpc_writer(output);
    let pump_stop = tokio_util::sync::CancellationToken::new();

    // A completion response is queued only after its final events, so a
    // subsequent prompt cannot overtake the previous run's agent_end.
    let (event_flush_tx, mut event_flush_rx) =
        tokio::sync::mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<()>>();
    // Stream fine-grained AgentEvents as JSONL until the receiver closes.
    let event_task = agent_events.take().map(|mut rx| {
        let out = stdout.clone();
        let stop = pump_stop.clone();
        tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    _ = stop.cancelled() => {
                        drain_rpc_events(&out, &mut rx).await;
                        break;
                    }
                    Some(done) = event_flush_rx.recv() => {
                        drain_rpc_events(&out, &mut rx).await;
                        let _ = done.send(());
                        continue;
                    }
                    event = rx.recv() => event,
                };
                match received {
                    Ok(event) => write_line(&out, &agent_event_json(&event)).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    });

    let event_flush = event_task.as_ref().map(|_| event_flush_tx);

    // Bridge the extension `UiDialog` mailbox over the rpc channel so a
    // permission-gate extension can ask the client a question mid-run: we emit
    // `{"type":"ui_dialog_request",…}` and answer it from a
    // `{"type":"ui_dialog_response",…}` on stdin. Without this a headless run
    // leaves the mailbox detached and every `UiDialog` call fails.
    ui_dialog.attach();
    let dialog_task = {
        let mailbox = ui_dialog.clone();
        let out = stdout.clone();
        let stop = pump_stop.clone();
        tokio::spawn(async move {
            let mut shown = std::collections::HashSet::new();
            while !stop.is_cancelled() {
                while let Some(request) = mailbox.take_pending() {
                    let event = crate::remote::protocol::RemoteEvent::UiDialogRequest {
                        request_id: request.request_id.clone(),
                        tool_call_id: request.tool_call_id,
                        ui: request.ui,
                    };
                    write_line(&out, &serde_json::to_value(event).unwrap()).await;
                    shown.insert(request.request_id);
                }
                let closed: Vec<String> = shown
                    .iter()
                    .filter(|id| !mailbox.is_pending(id))
                    .cloned()
                    .collect();
                for request_id in closed {
                    shown.remove(&request_id);
                    let event = crate::remote::protocol::RemoteEvent::UiDialogClosed { request_id };
                    write_line(&out, &serde_json::to_value(event).unwrap()).await;
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
                }
            }
        })
    };

    // A `prompt` runs in a spawned task so this loop keeps reading stdin while
    // the run is active — otherwise `abort` and `ui_dialog_response` could not
    // be delivered mid-run. Only one prompt may be in flight at a time.
    let run_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut run_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut run_cancel = tokio_util::sync::CancellationToken::new();
    let mut cancel_on_exit = false;

    // Readiness marker: a host/client can wait for the first line.
    write_line(&stdout, &serde_json::json!({"type": "ready"})).await;

    let mut lines = input.lines();
    loop {
        let incoming = tokio::select! {
            _ = writer_failed.cancelled() => {
                cancel_on_exit = true;
                break;
            }
            line = lines.next_line() => line,
        };
        let line = match incoming {
            Ok(Some(line)) => line,
            Ok(None) => break, // EOF
            Err(error) => {
                eprintln!("[ki] rpc stdin error: {error}");
                cancel_on_exit = true;
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                write_line(
                    &stdout,
                    &RemoteResponse::error(
                        serde_json::Value::Null,
                        format!("invalid json: {error}"),
                    )
                    .to_json(),
                )
                .await;
                continue;
            }
        };
        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let kind = msg
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        match kind.as_str() {
            "ui_dialog_response" => {
                let request_id = msg
                    .get("requestId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let result = if msg
                    .get("cancelled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    ui_dialog.cancel(&request_id)
                } else {
                    let answer = msg
                        .get("answer")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    ui_dialog.respond(&request_id, answer)
                };
                if msg.get("id").is_some() {
                    let response = match result {
                        Ok(()) => RemoteResponse::ok(
                            id,
                            serde_json::json!({"accepted":true,"requestId":request_id}),
                        ),
                        Err(error) => RemoteResponse::error(id, error),
                    };
                    write_line(&stdout, &response.to_json()).await;
                } else if let Err(error) = result {
                    eprintln!("[ki] ui_dialog_response: {error}");
                }
                continue;
            }
            "stop" => {
                cancel_on_exit = true;
                write_line(
                    &stdout,
                    &RemoteResponse::ok(id.clone(), serde_json::json!({"stopped": true})).to_json(),
                )
                .await;
                break;
            }
            "prompt" => {
                if run_active.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    write_line(
                        &stdout,
                        &RemoteResponse::error(id.clone(), "a run is already active").to_json(),
                    )
                    .await;
                    continue;
                }
                if let Some(task) = run_task.take() {
                    let _ = task.await;
                }
                run_cancel = tokio_util::sync::CancellationToken::new();
                let cancel_task = run_cancel.clone();
                let content = msg
                    .get("content")
                    .or_else(|| msg.get("prompt"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                let lane_task = Arc::clone(&lane);
                let out_task = stdout.clone();
                let active_task = Arc::clone(&run_active);
                let mailbox_task = ui_dialog.clone();
                let id_task = id.clone();
                let flush_events = event_flush.clone();
                run_task = Some(tokio::spawn(async move {
                    let result = match content {
                        Some(content) => {
                            rpc_prompt_cancellable(&lane_task, &content, cancel_task).await
                        }
                        None => Err("prompt requires a string `content`".to_string()),
                    };
                    let response = match result {
                        Ok(value) => RemoteResponse::ok(id_task, value),
                        Err(error) => RemoteResponse::error(id_task, error),
                    };
                    if let Some(flush) = flush_events {
                        let (done, drained) = tokio::sync::oneshot::channel();
                        if flush.send(done).is_ok() {
                            let _ = drained.await;
                        }
                    }
                    write_line(&out_task, &response.to_json()).await;
                    // A finished/aborted run leaves no dialog outstanding.
                    mailbox_task.cancel_all();
                    active_task.store(false, std::sync::atomic::Ordering::Release);
                }));
                continue;
            }
            _ => {}
        }

        let result = match kind.as_str() {
            "abort" if run_active.load(std::sync::atomic::Ordering::Acquire) => {
                // This also covers abort arriving before the task enters the lane.
                run_cancel.cancel();
                Ok(serde_json::json!({"aborted": true}))
            }
            "abort" => lane
                .abort()
                .await
                .map(|_| serde_json::json!({"aborted": true}))
                .map_err(|e| e.to_string()),
            "get_state" => Ok(rpc_get_state(&lane, harness).await),
            "set_model" => rpc_set_model(&lane, &model_catalog, &msg).await,
            "set_thinking_level" => rpc_set_thinking_level(&lane, &msg).await,
            "set_active_tools" => rpc_set_active_tools(&lane, &msg).await,
            "ping" => Ok(serde_json::json!({"pong": true})),
            other => Err(format!("unknown command type: {other}")),
        };

        let response = match result {
            Ok(value) => RemoteResponse::ok(id.clone(), value),
            Err(error) => RemoteResponse::error(id.clone(), error),
        };
        write_line(&stdout, &response.to_json()).await;
    }

    // EOF closes the input half of the protocol, not an accepted prompt.
    // There can be no more dialog answers, so release approvals, then await the
    // run before SessionShutdown can unload plugins or the runtime exits.
    ui_dialog.detach();
    if cancel_on_exit {
        run_cancel.cancel();
    }
    let mut exit_code = 0;
    if let Some(task) = run_task {
        if let Err(error) = task.await {
            eprintln!("[ki] rpc prompt task failed: {error}");
            exit_code = 1;
        }
    }
    pump_stop.cancel();
    let _ = dialog_task.await;
    if let Some(task) = event_task {
        let _ = task.await;
    }
    // All producers have joined; close the queue and flush every queued line.
    drop(stdout);
    match writer_task.await {
        Ok(Ok(())) => {}
        error => {
            eprintln!("[ki] rpc output failed: {error:?}");
            exit_code = 1;
        }
    }
    exit_code
}

#[derive(Clone)]
struct RpcWriter(tokio::sync::mpsc::Sender<Vec<u8>>);

fn start_rpc_writer<W: Write + Send + 'static>(
    mut output: W,
) -> (
    RpcWriter,
    tokio::task::JoinHandle<std::io::Result<()>>,
    tokio_util::sync::CancellationToken,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let failed = tokio_util::sync::CancellationToken::new();
    let failure_signal = failed.clone();
    let task = tokio::task::spawn_blocking(move || {
        let result = (|| {
            while let Some(line) = rx.blocking_recv() {
                output.write_all(&line)?;
                output.flush()?;
            }
            output.flush()
        })();
        if result.is_err() {
            failure_signal.cancel();
        }
        result
    });
    (RpcWriter(tx), task, failed)
}

async fn write_line(out: &RpcWriter, value: &serde_json::Value) {
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    let _ = out.0.send(line).await;
}

async fn drain_rpc_events(
    out: &RpcWriter,
    events: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
) {
    loop {
        match events.try_recv() {
            Ok(event) => write_line(out, &agent_event_json(&event)).await,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}

/// Cancellation is delivered even if abort/stop arrives before prompt_text has
/// registered a run. Once active, await its normal aborted outcome and cleanup.
async fn rpc_prompt_cancellable(
    lane: &Arc<dyn AgentLane>,
    content: &str,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<serde_json::Value, String> {
    let prompt = rpc_prompt_text(lane, content);
    tokio::pin!(prompt);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            match lane.abort().await {
                Ok(_) => prompt.await,
                Err(_) => Err("Operation aborted".to_string()),
            }
        }
        result = &mut prompt => result,
    }
}

fn rpc_outcome_str(outcome: &HarnessRunOutcome) -> (&'static str, Option<String>) {
    match outcome {
        HarnessRunOutcome::Completed { final_message, .. } => {
            ("completed", Some(assistant_text(final_message)))
        }
        HarnessRunOutcome::Aborted { final_message, .. } => {
            ("aborted", Some(assistant_text(final_message)))
        }
        HarnessRunOutcome::Failed { final_message, .. } => {
            ("failed", final_message.as_ref().map(assistant_text))
        }
        HarnessRunOutcome::Suspended { .. } => ("suspended", None),
    }
}

async fn rpc_prompt_text(
    lane: &Arc<dyn AgentLane>,
    content: &str,
) -> Result<serde_json::Value, String> {
    let result = lane
        .prompt_text(content, Vec::new())
        .await
        .map_err(|e| e.to_string())?;
    let (outcome, final_text) = rpc_outcome_str(&result.outcome);
    Ok(serde_json::json!({"outcome": outcome, "finalText": final_text}))
}

async fn rpc_get_state(lane: &Arc<dyn AgentLane>, harness: &AgentHarness) -> serde_json::Value {
    let model = lane.get_model().await.ok().map(|m| m.id);
    let thinking = lane
        .get_thinking_level()
        .await
        .ok()
        .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null));
    let active_tools = lane.get_active_tools().await.unwrap_or_default();
    let leaf_id = lane.get_leaf_id().await.ok().flatten();
    serde_json::json!({
        "model": model,
        "thinkingLevel": thinking,
        "activeTools": active_tools,
        "leafId": leaf_id,
        "sessionId": harness.session().storage().metadata().id,
    })
}

async fn rpc_set_model(
    lane: &Arc<dyn AgentLane>,
    catalog: &[rpi_ai::Model],
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = msg
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("set_model requires `model`")?;
    let model = catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(id))
        .cloned()
        .ok_or_else(|| format!("unknown model: {id}"))?;
    lane.set_model(model).await.map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"model": id}))
}

async fn rpc_set_thinking_level(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let value = msg
        .get("level")
        .or_else(|| msg.get("thinkingLevel"))
        .ok_or("set_thinking_level requires `level`")?;
    let level: rpi_ai::types::ThinkingLevel =
        serde_json::from_value(value.clone()).map_err(|_| {
            "invalid thinking level (off|minimal|low|medium|high|xhigh|max)".to_string()
        })?;
    lane.set_thinking_level(level)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"thinkingLevel": level}))
}

async fn rpc_set_active_tools(
    lane: &Arc<dyn AgentLane>,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let tools = msg
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .ok_or("set_active_tools requires a `tools` array")?;
    lane.set_active_tools(tools.clone())
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"activeTools": tools}))
}

/// Stable JSON projection for the fine-grained agent lifecycle stream.
///
/// Delegates to the shared [`crate::remote::protocol::RemoteEvent`] so the
/// server and the `--connect` client agree on the wire shape by construction.
fn agent_event_json(event: &AgentEvent) -> serde_json::Value {
    serde_json::to_value(crate::remote::protocol::RemoteEvent::from_agent_event(
        event,
    ))
    .unwrap_or(serde_json::Value::Null)
}

/// Emit a single harness event as a JSON line on stdout. Mirrors the TS
/// `toJsonEvent` projection (here a lossy but stable shape: `type` + the event
/// payload's key fields).
fn emit_json_event(event: &HarnessEvent) {
    let line = match event {
        HarnessEvent::CompactionProgress(e) => {
            serde_json::json!({ "type": "compaction_progress", "lane": e.lane, "manual": e.manual, "active": e.active })
        }
        HarnessEvent::RunStart(e) => serde_json::json!({
            "type": "run_start",
            "lane": e.lane,
            "runId": e.run_id,
        }),
        HarnessEvent::RunEnd(e) => serde_json::json!({
            "type": "run_end",
            "lane": e.lane,
            "runId": e.run_id,
            "outcome": run_end_outcome_str(e.outcome),
            "leafId": e.leaf_id,
        }),
    };
    println!("{line}");
}

fn run_end_outcome_str(o: RunEndOutcome) -> &'static str {
    match o {
        RunEndOutcome::Completed => "completed",
        RunEndOutcome::Aborted => "aborted",
        RunEndOutcome::Failed => "failed",
    }
}

/// `interactive` mode: uses TUI if terminal supports it, falls back to minimal REPL.
///
/// `event_rx` carries the live `AgentEvent` stream (drained by the TUI to
/// render streaming responses). The REPL fallback ignores it.
///
/// `model_catalog` is the resolved provider's full model list, passed through
/// so the TUI's `/model` selector can display available models (read-only —
/// v1 does not switch models mid-session; see the initial port notes (retired)).
pub async fn interactive(
    harness: &AgentHarness,
    event_rx: Option<tokio::sync::broadcast::Receiver<rpi_agent::AgentEvent>>,
    args: &Args,
    model_catalog: Vec<rpi_ai::Model>,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
    theme: Option<&str>,
    no_themes: bool,
    reload_context: &crate::session::ReloadContext,
) -> i32 {
    // ---- Session lifecycle (P1): BeforeTuiStart ----
    // `BeforeTuiStart`: extensions may prepare (connect a server, load a
    // resource) before the interactive UI initializes. Runs with a per-event
    // timeout; a handler returning `EVENT_HANDLER_ABORT` **vetoes** startup —
    // we print the reason and exit without entering the TUI. No-op when no
    // extension subscribes.
    if let Some(reason) = crate::session::dispatch_session_event_async(
        reload_context,
        rpi_plugin_sdk::EventTag::BeforeTuiStart,
    )
    .await
    {
        eprintln!("[ki] startup vetoed by extension: {reason}");
        return crate::app::EXIT_VETOED;
    }

    // Check if TUI is supported
    let force_tui = std::env::var("KI_FORCE_TUI")
        .map(|v| v == "1")
        .unwrap_or(false);
    let code = if force_tui || crate::interactive_tui::is_tui_supported() {
        // Use TUI-based interactive mode
        crate::interactive_tui::interactive_tui(
            harness,
            event_rx,
            args,
            model_catalog,
            initial,
            extra_messages,
            initial_images,
            theme,
            no_themes,
            reload_context,
        )
        .await
    } else {
        // Fall back to simple REPL
        interactive_repl(harness, args, initial, extra_messages, initial_images).await
    };

    // `SessionShutdown`: the interactive session closed (TUI/REPL returned) —
    // extensions release resources, disconnect, persist state. Fired after the
    // UI returns so it always runs (even on early-return paths inside the TUI).
    // A veto here is **advisory** (the session is already closing) — warn and
    // continue.
    if let Some(reason) = crate::session::dispatch_session_event_async(
        reload_context,
        rpi_plugin_sdk::EventTag::SessionShutdown,
    )
    .await
    {
        tracing::warn!(
            "[ki] extension vetoed SessionShutdown (ignored, session closing): {reason}"
        );
    }

    code
}

/// Simple REPL-based interactive mode (fallback for non-TTY environments).
pub async fn interactive_repl(
    harness: &AgentHarness,
    #[allow(unused_variables)] args: &Args,
    initial: Option<String>,
    extra_messages: &[String],
    initial_images: Vec<ImageContent>,
) -> i32 {
    // Debug: confirm we entered REPL mode
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let stdin = std::io::stdin();
    let is_tty = stdin.is_terminal();

    if is_tty {
        println!("ki interactive (v1 minimal REPL). Type /exit to quit, /abort to cancel a run.\n");
    }

    // Run the initial prompt + extra messages first (same as print mode).
    let mut prompts: Vec<String> = Vec::new();
    if let Some(init) = initial {
        prompts.push(init);
    }
    for m in extra_messages {
        prompts.push(m.clone());
    }
    let mut images = initial_images;
    for prompt in prompts {
        if let Err(code) = run_one(&lane, &prompt, std::mem::take(&mut images)).await {
            return code;
        }
    }

    // Then read lines from stdin until EOF / `/exit`.
    let mut line = String::new();
    loop {
        if is_tty {
            print!("> ");
            let _ = std::io::stdout().flush();
        }
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }
        if trimmed == "/abort" {
            let _ = lane.abort().await;
            eprintln!("(aborted)");
            continue;
        }
        if let Err(code) = run_one(&lane, trimmed, Vec::new()).await {
            return code;
        }
    }
    0
}

/// Run a single prompt in interactive mode, printing the assistant reply (or
/// the error). Returns `Ok(())` on success/soft-failure, `Err(exit_code)` on a
/// hard rejection.
async fn run_one(
    lane: &Arc<dyn AgentLane>,
    prompt: &str,
    images: Vec<ImageContent>,
) -> Result<(), i32> {
    match lane.prompt_text(prompt, images).await {
        Ok(result) => {
            match &result.outcome {
                HarnessRunOutcome::Completed { final_message, .. }
                | HarnessRunOutcome::Aborted { final_message, .. } => {
                    let text = assistant_text(final_message);
                    if !text.is_empty() {
                        println!("{text}");
                    }
                }
                HarnessRunOutcome::Failed {
                    error,
                    final_message,
                    ..
                } => {
                    if let Some(m) = final_message {
                        if let Some(em) = &m.error_message {
                            eprintln!("error: {em}");
                        }
                    }
                    eprintln!("run failed: {error:?}");
                }
                HarnessRunOutcome::Suspended { .. } => {
                    eprintln!("run suspended (deferred) — resume not supported in v1");
                }
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("prompt rejected: {e}");
            Err(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::events::AgentEvent;
    use rpi_agent::types::AgentToolResult;
    use rpi_ai::types::{
        AssistantMessage, Content, StopReason, TextContent, TextContentType, Usage,
    };
    use rpi_harness::session::types::OperationError;

    #[derive(Clone, Default)]
    struct RpcOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for RpcOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl RpcOutput {
        fn messages(&self) -> Vec<serde_json::Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        async fn wait_for(
            &self,
            predicate: impl Fn(&serde_json::Value) -> bool,
        ) -> serde_json::Value {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if let Some(message) = self.messages().into_iter().find(&predicate) {
                        return message;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("RPC message not received")
        }
    }

    async fn rpc_test_harness(
        text: &str,
        tokens_per_second: f64,
    ) -> (AgentHarness, Arc<rpi_ai::providers::faux::FauxProvider>) {
        let provider = rpi_ai::providers::faux::FauxProvider::new(
            rpi_ai::providers::faux::FauxScript::new()
                .with_text(text)
                .with_tokens_per_second(tokens_per_second),
        );
        let options = rpi_harness::types::AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider.clone()],
            ..Default::default()
        };
        (AgentHarness::create(options).await.unwrap(), provider)
    }

    fn start_test_rpc(
        harness: AgentHarness,
        ui: rpi_extensions::UiDialogMailbox,
    ) -> (
        tokio::io::DuplexStream,
        RpcOutput,
        tokio::task::JoinHandle<i32>,
    ) {
        let (client, input) = tokio::io::duplex(4096);
        let output = RpcOutput::default();
        let captured = output.clone();
        let task = tokio::spawn(async move {
            rpc_with_io(
                &harness,
                None,
                vec![],
                ui,
                tokio::io::BufReader::new(input),
                captured,
            )
            .await
        });
        (client, output, task)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_eof_waits_for_accepted_prompt() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("completed", 100.0).await;
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"content\":\"test\"}\n")
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let response = output
            .messages()
            .into_iter()
            .find(|m| m["id"] == 1)
            .unwrap();
        assert_eq!(response["result"]["outcome"], "completed");
        assert_eq!(response["result"]["finalText"], "completed");
        assert!(!ui.is_attached());
    }

    #[tokio::test]
    async fn rpc_eof_drains_queued_final_events() {
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        for _ in 0..100 {
            tx.send(AgentEvent::AgentStart).unwrap();
        }
        tx.send(AgentEvent::AgentEnd { messages: vec![] }).unwrap();
        let output = RpcOutput::default();
        let code = rpc_with_io(
            &harness,
            Some(rx),
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            tokio::io::BufReader::new(&b""[..]),
            output.clone(),
        )
        .await;
        assert_eq!(code, 0);
        let messages = output.messages();
        assert_eq!(
            messages.len(),
            102,
            "ready plus every queued event must be flushed"
        );
        assert_eq!(messages.last().unwrap()["type"], "agent_end");
    }

    struct BackpressuredOutput {
        entered: Arc<std::sync::atomic::AtomicBool>,
        release: Arc<std::sync::atomic::AtomicBool>,
        output: RpcOutput,
    }

    impl Write for BackpressuredOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.entered
                .store(true, std::sync::atomic::Ordering::SeqCst);
            while !self.release.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.output.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_output_backpressure_is_bounded_and_keeps_workers_responsive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let release = Arc::new(AtomicBool::new(false));
        struct ReleaseOnDrop(Arc<AtomicBool>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _release = ReleaseOnDrop(release.clone());
        let entered = Arc::new(AtomicBool::new(false));
        let output = RpcOutput::default();
        let (writer, task, _) = start_rpc_writer(BackpressuredOutput {
            entered: entered.clone(),
            release: release.clone(),
            output: output.clone(),
        });
        write_line(&writer, &serde_json::json!({"id": 0})).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let producer_writer = writer.clone();
        let producer = tokio::spawn(async move {
            for id in 1..=100 {
                write_line(&producer_writer, &serde_json::json!({"id": id})).await;
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_millis(150),
            tokio::time::sleep(std::time::Duration::from_millis(20)),
        )
        .await
        .unwrap();
        assert!(
            !producer.is_finished(),
            "the bounded queue must apply backpressure"
        );
        release.store(true, Ordering::SeqCst);
        producer.await.unwrap();
        drop(writer);
        task.await.unwrap().unwrap();
        let messages = output.messages();
        assert_eq!(messages.len(), 101);
        for (index, message) in messages.iter().enumerate() {
            assert_eq!(message["id"], index);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_completion_response_follows_final_event_under_backpressure() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (harness, _) = rpc_test_harness("completed", 0.0).await;
        let (emitter, events) = rpi_agent::events::BroadcastEmitter::new(256);
        harness
            .set_agent_emitter(Some(Arc::new(emitter)))
            .await
            .unwrap();
        let output = RpcOutput::default();
        let release = Arc::new(AtomicBool::new(false));
        let release_writer = release.clone();
        let release_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            release_writer.store(true, Ordering::SeqCst);
        });
        let code = rpc_with_io(
            &harness,
            Some(events),
            vec![],
            rpi_extensions::UiDialogMailbox::new(),
            tokio::io::BufReader::new(
                &b"{\"type\":\"prompt\",\"id\":1,\"content\":\"test\"}\n"[..],
            ),
            BackpressuredOutput {
                entered: Arc::new(AtomicBool::new(false)),
                release,
                output: output.clone(),
            },
        )
        .await;
        release_thread.join().unwrap();
        assert_eq!(code, 0);
        let messages = output.messages();
        let end = messages
            .iter()
            .position(|m| m["type"] == "agent_end")
            .unwrap();
        let response = messages.iter().position(|m| m["id"] == 1).unwrap();
        assert!(
            end < response,
            "a new prompt must not overtake the previous agent_end"
        );
        assert_eq!(messages[response]["result"]["outcome"], "completed");
    }

    #[tokio::test]
    async fn rpc_output_failure_closes_without_waiting_for_more_input() {
        struct BrokenOutput;
        impl Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed client output",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let (_client, input) = tokio::io::duplex(4096);
        let ui = rpi_extensions::UiDialogMailbox::new();
        let code = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            rpc_with_io(
                &harness,
                None,
                vec![],
                ui.clone(),
                tokio::io::BufReader::new(input),
                BrokenOutput,
            ),
        )
        .await
        .unwrap();
        assert_eq!(code, 1);
        assert!(!ui.is_attached());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_stop_aborts_and_joins_active_prompt() {
        use tokio::io::AsyncWriteExt;
        let (harness, provider) = rpc_test_harness("still streaming the response", 20.0).await;
        let lane = harness.lane("main");
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"content\":\"test\"}\n")
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while provider
                .state()
                .call_count
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        client
            .write_all(b"{\"type\":\"stop\",\"id\":2}\n")
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let messages = output.messages();
        assert_eq!(
            messages.iter().find(|m| m["id"] == 1).unwrap()["result"]["outcome"],
            "aborted"
        );
        assert_eq!(
            messages.iter().find(|m| m["id"] == 2).unwrap()["result"]["stopped"],
            true
        );
        assert!(
            lane.abort().await.is_err(),
            "RPC returned with an active lane"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_immediate_stop_cancels_prompt_before_or_after_lane_start() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) = rpc_test_harness("not completed", 20.0).await;
        let lane = harness.lane("main");
        let (mut client, output, task) =
            start_test_rpc(harness, rpi_extensions::UiDialogMailbox::new());
        client.write_all(b"{\"type\":\"prompt\",\"id\":1,\"content\":\"test\"}\n{\"type\":\"stop\",\"id\":2}\n").await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let response = output
            .messages()
            .into_iter()
            .find(|m| m["id"] == 1)
            .unwrap();
        assert!(
            response["result"]["outcome"] == "aborted" || response["error"] == "Operation aborted",
            "{response}"
        );
        assert!(lane.abort().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_dialog_answers_and_ping_are_read_while_prompt_runs() {
        use tokio::io::AsyncWriteExt;
        let (harness, _) =
            rpc_test_harness("the streamed response finishes after the dialog", 30.0).await;
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        client
            .write_all(b"{\"type\":\"prompt\",\"id\":1,\"content\":\"test\"}\n")
            .await
            .unwrap();
        ui.handle(serde_json::json!({"requestId":"approval", "toolCallId":"call", "ui":{}}))
            .unwrap();
        output.wait_for(|m| m["type"] == "ui_dialog_request").await;
        client.write_all(b"{\"type\":\"ui_dialog_response\",\"requestId\":\"approval\",\"answer\":true}\n{\"type\":\"ping\",\"id\":2}\n").await.unwrap();
        output.wait_for(|m| m["id"] == 2).await;
        assert_eq!(ui.poll("approval").unwrap()["answer"], true);
        output
            .wait_for(|m| m["type"] == "ui_dialog_closed" && m["requestId"] == "approval")
            .await;
        client.shutdown().await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(
            output
                .messages()
                .into_iter()
                .find(|m| m["id"] == 1)
                .unwrap()["result"]["outcome"],
            "completed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_notifies_client_when_an_approval_is_cancelled() {
        use tokio::io::AsyncWriteExt;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        ui.handle(serde_json::json!({"requestId":"cancelled", "toolCallId":"c", "ui":{}}))
            .unwrap();
        output.wait_for(|m| m["type"] == "ui_dialog_request").await;
        ui.cancel_tool_call("c");
        output
            .wait_for(|m| m["type"] == "ui_dialog_closed" && m["requestId"] == "cancelled")
            .await;
        // The notification must not consume the cancellation meant for the plugin.
        assert_eq!(ui.poll("cancelled").unwrap()["status"], "cancelled");
        client.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    #[tokio::test]
    async fn rpc_accepts_native_dialog_responses_without_consuming_plugin_answers() {
        use crate::remote::protocol::RemoteCommand;
        use tokio::io::AsyncWriteExt;
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let ui = rpi_extensions::UiDialogMailbox::new();
        let (mut client, output, task) = start_test_rpc(harness, ui.clone());
        output.wait_for(|m| m["type"] == "ready").await;
        for (request_id, answer, response_id) in [
            (
                "answered",
                Some(serde_json::json!({"values":["No"],"kind":"selector"})),
                5,
            ),
            ("cancelled", None, 6),
        ] {
            ui.handle(serde_json::json!({"requestId":request_id,"toolCallId":request_id,"ui":{}}))
                .unwrap();
            output
                .wait_for(|m| m["type"] == "ui_dialog_request" && m["requestId"] == request_id)
                .await;
            let command = RemoteCommand::UiDialogResponse {
                request_id: request_id.into(),
                cancelled: answer.is_none(),
                answer,
            }
            .with_id(response_id);
            client
                .write_all(format!("{command}\n").as_bytes())
                .await
                .unwrap();
            let ack = output.wait_for(|m| m["id"] == response_id).await;
            assert_eq!(ack["result"]["accepted"], true);
            let result = ui.poll(request_id).unwrap();
            if request_id == "answered" {
                assert_eq!(result["answer"]["values"][0], "No");
            } else {
                assert_eq!(result["status"], "cancelled");
            }
        }
        client.shutdown().await.unwrap();
        assert_eq!(task.await.unwrap(), 0);
    }

    fn assistant(text: &str, stop: StopReason) -> AssistantMessage {
        AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![Content::Text(TextContent {
                kind: TextContentType,
                text: text.into(),
                text_signature: None,
            })],
            api: rpi_ai::Api::AnthropicMessages,
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }
    }

    #[test]
    fn assistant_text_concatenates_text_blocks() {
        let m = assistant("hello", StopReason::Stop);
        assert_eq!(assistant_text(&m), "hello");
    }

    #[test]
    fn outcome_exit_code_maps_failed_aborted_to_1() {
        let failed = HarnessRunOutcome::Failed {
            leaf_id: "l".into(),
            error: OperationError {
                code: "boom".into(),
                message: "boom".into(),
            },
            final_entry_id: None,
            final_message: None,
        };
        assert_eq!(outcome_exit_code(&failed), 1);
        let completed = HarnessRunOutcome::Completed {
            leaf_id: "l".into(),
            final_entry_id: "e".into(),
            final_message: assistant("ok", StopReason::Stop),
        };
        assert_eq!(outcome_exit_code(&completed), 0);
    }

    #[test]
    fn run_end_outcome_str_roundtrip() {
        assert_eq!(run_end_outcome_str(RunEndOutcome::Completed), "completed");
        assert_eq!(run_end_outcome_str(RunEndOutcome::Aborted), "aborted");
        assert_eq!(run_end_outcome_str(RunEndOutcome::Failed), "failed");
    }

    #[test]
    fn agent_event_projection_keeps_terminal_and_tool_payloads() {
        let end = agent_event_json(&AgentEvent::AgentEnd { messages: vec![] });
        assert_eq!(end["type"], "agent_end");
        assert_eq!(end["messages"], serde_json::json!([]));

        let tool = agent_event_json(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            result: AgentToolResult::text("hello"),
            is_error: false,
        });
        assert_eq!(tool["type"], "tool_execution_end");
        assert_eq!(tool["result"]["content"][0]["text"], "hello");
        assert_eq!(tool["result"]["terminate"], false);

        let retry = agent_event_json(&AgentEvent::RetryScheduled {
            attempt: 3,
            max_retries: 10,
            delay_ms: 8_000,
            error: "503 service unavailable".into(),
        });
        assert_eq!(retry["type"], "retry_scheduled");
        assert_eq!(retry["attempt"], 3);
        assert_eq!(retry["maxRetries"], 10);
        assert_eq!(retry["delayMs"], 8_000);
        assert_eq!(retry["error"], "503 service unavailable");
    }
}
