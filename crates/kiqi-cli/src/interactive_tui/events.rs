//! Agent event rendering and queued-message recovery.

use super::*;

// ===========================================================================
// AgentEvent drain task — the streaming core
// ===========================================================================

/// Drain `AgentEvent`s from the broadcast receiver and apply the TS
/// `handleEvent` event→UI mapping. Runs on a `tokio::spawn`'d task for the
/// lifetime of the TUI.
/// Re-read the lane's queue and update the pending-messages dock slot.
/// Cheap (two short mutex locks) and a no-op when the snapshot is unchanged,
/// so it is safe to call on every relevant event.
pub(super) async fn refresh_pending_messages(state: &Arc<TuiState>, lane: &Arc<dyn AgentLane>) {
    if let Ok(queued) = lane.queued_messages().await {
        state.set_pending_queue(queued);
    }
}

/// Whether the transcript holds a panel that repaints its elapsed readout from
/// the clock on every frame: a running bash command, or a tool call still in
/// [`ToolStatus::Running`]. Such a panel needs a real transcript rebuild each
/// render tick — a frame that reuses the cached scroll content would leave its
/// timer frozen at the second of the last full rebuild.
pub(super) fn transcript_has_live_panel(
    bash_components_present: bool,
    tool_components: &HashMap<String, Arc<ToolExecutionComponent>>,
) -> bool {
    bash_components_present
        || tool_components
            .values()
            .any(|component| component.status() == ToolStatus::Running)
}

/// Prepend restored queued messages to the draft the user may already have
/// typed (upstream's `restoreQueuedMessagesToEditor` places them first).
pub(super) fn merge_queued_into_draft(queued_text: &str, current: &str) -> String {
    if current.trim().is_empty() {
        queued_text.to_string()
    } else {
        format!("{queued_text}\n\n{current}")
    }
}

/// Place the caret at the END of `text` after `set_text`. `set_text` resets the
/// caret to `(0, 0)`, and the last line's **byte** length is the right column:
/// a char count was wrong for multibyte drafts, and row 0 was wrong for a
/// multi-line one (upstream's `setTextInternal(text, "end")`).
pub(super) fn set_editor_text_caret_at_end(editor: &Arc<Editor>, text: &str) {
    editor.set_text(text);
    let last_row = text.split('\n').count().saturating_sub(1);
    let last_len = text.split('\n').next_back().map(str::len).unwrap_or(0);
    editor.set_cursor(last_row, last_len);
}

/// Drain every queued steering/follow-up message out of the lane and restore it
/// to the editor (upstream's `restoreQueuedMessagesToEditor`). Returns how many
/// were restored; the queue is consumed either way, so the dock drops its rows.
///
/// Runs on a spawned task rather than through the [`TuiMessage`] mailbox: the
/// async main loop owns `run_prompt_streaming(..).await` for the whole run, and
/// a queue only exists *while* a run is in flight — so a `TuiMessage::Dequeue`
/// was never serviced until the run ended, after the lane had already consumed
/// the queue. That made Alt+Q a no-op while it was most needed.
pub(super) async fn restore_queued_messages_to_editor(
    lane: Arc<dyn AgentLane>,
    editor: Arc<Editor>,
    state: Arc<TuiState>,
    tui: Arc<TuiAltScreen>,
) -> usize {
    match lane.clear_queue().await {
        Ok(queued) => {
            state.restore_pending_images(queued.images.clone());
            let all: Vec<String> = queued
                .steering
                .iter()
                .chain(queued.follow_up.iter())
                .cloned()
                .collect();
            if !all.is_empty() {
                let queued_text = all.join("\n\n");
                // Read the EXPANDED draft: `set_text` below clears the folded
                // paste store, so any `[paste #id …]` marker still in the
                // editor must be materialized first or it would be left behind
                // as literal text.
                let current = editor.get_expanded_text();
                let combined = merge_queued_into_draft(&queued_text, &current);
                set_editor_text_caret_at_end(&editor, &combined);
            }
            // The lane drained the queue; refresh so the dock drops the rows.
            refresh_pending_messages(&state, &lane).await;
            tui.request_render(false);
            all.len()
        }
        Err(error) => {
            add_error_message(
                &state.chat_container,
                &format!("Could not restore queued messages: {error}"),
            );
            tui.request_render(false);
            0
        }
    }
}

/// `app.message.dequeue`: restore the queued messages and report the count, like
/// upstream's `handleDequeue` (`Restored N queued message(s)` / `No queued
/// messages to restore`). The status is a dim transcript note (ki's analog of
/// native `showStatus`).
pub(super) async fn handle_dequeue(
    lane: Arc<dyn AgentLane>,
    editor: Arc<Editor>,
    state: Arc<TuiState>,
    tui: Arc<TuiAltScreen>,
) {
    let restored =
        restore_queued_messages_to_editor(lane, editor, state.clone(), tui.clone()).await;
    if restored == 0 {
        add_note_message(&state.chat_container, "No queued messages to restore");
    } else {
        let plural = if restored > 1 { "s" } else { "" };
        add_note_message(
            &state.chat_container,
            &format!("Restored {restored} queued message{plural} to editor"),
        );
    }
    tui.request_render(false);
}

/// Cancel the active run the native-pi way: pull every queued steering /
/// follow-up message back into the editor FIRST, then abort. upstream's
/// `onEscape` does exactly this during streaming
/// (`restoreQueuedMessagesToEditor({abort: true})`), so a cancel never silently
/// swallows messages the user staged while the run was in flight — they land in
/// the editor to be re-edited or resent.
pub(super) fn abort_run_restoring_queue(
    lane: Arc<dyn AgentLane>,
    editor: Arc<Editor>,
    state: Arc<TuiState>,
    tui: Arc<TuiAltScreen>,
    wake_state: Arc<crate::signal_host::SignalHost>,
) {
    // Pressing Esc must not be followed by the agent getting up again on its
    // own: mute signal-triggered wakes until the user submits something.
    wake_state.record_abort();
    tokio::spawn(async move {
        let _ = restore_queued_messages_to_editor(lane.clone(), editor, state, tui.clone()).await;
        let _ = lane.abort().await;
        tui.request_render(false);
    });
}

/// The dim transcript note a soft interrupt leaves behind, per
/// `soft-interrupt.md` §3.5. Two constants rather than one, because
/// [`InterruptResult::Soft`] now carries the [`ActionKind`] the press actually
/// reached: "the current tool call" and "the reply" are different things to the
/// user, and a single note had to generalise to "step", which named neither.
///
/// Both must hold for both continuations: with queued input the loop injects it
/// and asks again, without it the turn ends and the run survives. So "queued
/// input will run next", and never "abort".
///
/// The `NothingToInterrupt` note must not claim that nothing happened: the soft
/// flag is latched in that arm, so the *next* action the run installs starts
/// cancelled (`InterruptResult::NothingToInterrupt` documents this). A note that
/// said "nothing to interrupt" would be lying to the user one edit later.
pub(super) const SOFT_INTERRUPT_TOOL_NOTE: &str =
    "Interrupted the current tool call; queued input will run next.";
pub(super) const SOFT_INTERRUPT_STREAM_NOTE: &str =
    "Interrupted the reply; queued input will run next.";
pub(super) const NOTHING_TO_INTERRUPT_NOTE: &str =
    "Nothing in flight right now; the next step will be stopped.";
pub(super) const NO_ACTIVE_RUN_TO_INTERRUPT_NOTE: &str = "No run to interrupt.";

/// Pick the note for the action the press reached.
///
/// Kept as a pure function of [`ActionKind`] so the wording can be asserted
/// without driving a whole key press, and so there is exactly one place that
/// maps "what did I stop" to "what do I say".
pub(super) fn soft_interrupt_note(action: ActionKind) -> &'static str {
    match action {
        ActionKind::Tool => SOFT_INTERRUPT_TOOL_NOTE,
        ActionKind::Stream => SOFT_INTERRUPT_STREAM_NOTE,
    }
}

/// `app.interrupt` when Esc is a *single* press: cancel the action in flight and
/// let the run continue. Deliberately does none of the hard-abort side effects —
/// no `Aborting` status, no `record_abort()` (the signal/goal chains stay live,
/// I5), and no queue restoration (the messages stay queued, I3: the loop drains
/// them at its next safe point). `lane.interrupt()` is async and the key loop is
/// a blocking crossterm reader, so this runs on a spawned task like
/// [`abort_run_restoring_queue`].
pub(super) fn soft_interrupt_run(
    lane: Arc<dyn AgentLane>,
    state: Arc<TuiState>,
    tui: Arc<TuiAltScreen>,
) {
    tokio::spawn(async move {
        let note = match lane.interrupt().await {
            Ok(InterruptResult::Soft { action }) => soft_interrupt_note(action),
            Ok(InterruptResult::NothingToInterrupt) => NOTHING_TO_INTERRUPT_NOTE,
            Ok(InterruptResult::NoActiveRun) => NO_ACTIVE_RUN_TO_INTERRUPT_NOTE,
            Err(error) => {
                add_error_message(
                    &state.chat_container,
                    &format!("Could not interrupt the run: {error}"),
                );
                tui.request_render(false);
                return;
            }
        };
        // A dim note in the transcript, the same mechanism `handle_dequeue` and
        // `add_error_message` use: it lands above the dock and never steals a
        // layout slot, so a status line is never overwritten mid-render.
        add_note_message(&state.chat_container, note);
        tui.request_render(false);
    });
}

/// Drain MCP notices at a boundary and, when the policy allows, emit the wake
/// trigger. Shared by `AgentEnd` and the idle pump so both paths use exactly
/// one policy and one ordering (record first, then decide).
pub(super) async fn handle_signal_boundary(
    harness: &AgentHarness,
    mcp: &Arc<kiqi_mcp::McpManager>,
    wake_state: &Arc<crate::signal_host::SignalHost>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
) -> bool {
    let batch = kiqi_mcp::boundary::take_mcp_boundary(harness, mcp).await;
    if batch.is_empty() {
        return false;
    }
    kiqi_mcp::boundary::append_notice_batch(harness, &batch).await;
    let any_auto = batch.notices.iter().any(|notice| {
        mcp.signal_config_for(&notice.server, &notice.uri)
            .is_some_and(|config| config.wake == kiqi_mcp::WakeMode::Auto)
    });
    let decision = wake_state.decide(&batch, std::time::Instant::now(), |server, uri| {
        mcp.signal_config_for(server, uri)
    });
    match decision {
        crate::signal_wake::WakeDecision::Wake(servers) => {
            // The inbox is the durable disposition surface: the agent can
            // defer/resolve/dismiss through the `signals` tool. A write failure
            // must not stop the wake — the notice entry is already on disk.
            let pending = match wake_state.add_to_inbox(&batch).await {
                Ok(count) => count,
                Err(error) => {
                    super::diagnostic_log(&format!(
                        "warning: mcp: could not record the signal inbox: {error}"
                    ));
                    0
                }
            };
            super::diagnostic_log(&format!(
                "[ki mcp] signal wake: servers={servers:?} notices={} suppressed={} inbox={pending}",
                batch.notices.len(),
                batch.suppressed
            ));
            // The main loop drives the run through the same streaming path as
            // a user prompt. A closed channel means the TUI is shutting down;
            // the notice is already recorded, so nothing is lost.
            if tx
                .send(TuiMessage::SignalWake(crate::signal_wake::wake_message(
                    &batch, pending,
                )))
                .is_err()
            {
                super::diagnostic_log(
                    "warning: mcp: signal wake dropped: the TUI channel is closed",
                );
            }
            true
        }
        crate::signal_wake::WakeDecision::Skip(reason) => {
            if any_auto {
                // Only `auto` servers produce a skip line: with the default
                // `off`, every notification would otherwise log, which is noise.
                let servers: Vec<&str> = batch.notices.iter().map(|n| n.server.as_str()).collect();
                super::diagnostic_log(&format!(
                    "[ki mcp] signal recorded without wake: {reason} (servers={servers:?} {})",
                    provenance_summary(&batch)
                ));
            }
            false
        }
    }
}

/// Whether the walgit board has no open cards, with a bounded wait so a hung
/// `walgit` cannot stall the AgentEnd boundary. Any failure is "not done":
/// auto-continue resumes normally rather than claiming a false completion.
pub(crate) async fn board_open_zero(cwd: &std::path::Path) -> bool {
    let mut command = tokio::process::Command::new("walgit");
    command
        .args(["collab", "board"])
        .current_dir(cwd)
        .kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(5), command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            crate::goal_drive::board_has_no_open_cards(&String::from_utf8_lossy(&output.stdout))
        }
        _ => false,
    }
}

/// Evaluate a goal's `doneWhenCmd` at AgentEnd: run the command in the
/// session cwd with a short timeout, under the same shell sandbox as the bash
/// tool (so it can run `walgit collab board` or tests but cannot rewrite the
/// permission policy). Exit 0 = done; timeout, spawn failure or a non-zero
/// exit = not done, so auto-continue resumes normally.
pub(crate) async fn done_when_cmd_satisfied(cwd: &std::path::Path, cmd: &str) -> bool {
    use std::time::Duration;

    let sandbox =
        kiqi_tools::sandbox::ShellSandbox::macos_for(crate::permissions::shell_sandbox_paths(cwd));
    let (program, args) = match sandbox.as_ref().and_then(|s| s.shell_override()) {
        Some(ov) => (ov.program, ov.args),
        None => {
            let shell = if std::path::Path::new("/bin/bash").exists() {
                "/bin/bash"
            } else {
                "/bin/sh"
            };
            (std::path::PathBuf::from(shell), vec!["-c".to_string()])
        }
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(&args)
        .arg(cmd)
        .current_dir(cwd)
        .kill_on_drop(true)
        .env_clear();
    // Only the lookup basics; the sandbox profile still confines writes, and
    // a goal definition must not smuggle host-private plumbing into a child.
    command.env(
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
    );
    if let Ok(home) = std::env::var("HOME") {
        command.env("HOME", home);
    }
    match tokio::time::timeout(Duration::from_secs(10), command.output()).await {
        Ok(Ok(output)) => output.status.success(),
        _ => false,
    }
}

/// After a run ends, keep an active goal moving. The goal extension injects the
/// goal into context but cannot start runs; without this the session would sit
/// idle between turns. A signal wake in the same boundary wins (its run already
/// carries the goal), and user input / Esc always take precedence.
pub(super) async fn maybe_continue_goal(
    lane: &Arc<dyn AgentLane>,
    wake_state: &Arc<crate::signal_host::SignalHost>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
) {
    if wake_state.muted_until_input() {
        return;
    }
    let user_waiting = lane
        .queued_messages()
        .await
        .map(|queued| !queued.steering.is_empty() || !queued.follow_up.is_empty())
        .unwrap_or(false);
    if user_waiting {
        return;
    }
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Some(goal) = crate::goal_drive::active_goal(&cwd) else {
        return;
    };
    if goal.done_when.as_deref() == Some("board-open-0") && board_open_zero(&cwd).await {
        super::diagnostic_log(
            "[ki goal] doneWhen 'board-open-0' satisfied; not auto-continuing \
             (run /goal stop to archive the goal)",
        );
        return;
    }
    if let Some(done_when_cmd) = goal.done_when_cmd.as_deref() {
        if done_when_cmd_satisfied(&cwd, done_when_cmd).await {
            super::diagnostic_log(&format!(
                "[ki goal] doneWhenCmd satisfied; not auto-continuing: {done_when_cmd}"
            ));
            return;
        }
    }
    if !goal.can_continue(crate::goal_drive::unix_millis()) {
        return;
    }
    let unbounded = goal.max_turns.is_none() && goal.deadline_ms.is_none();
    if !wake_state.allow_goal_continuation(&goal.key(), unbounded) {
        super::diagnostic_log(&format!(
            "[ki goal] auto-continue stopped: unbounded goal reached {} host runs",
            crate::goal_drive::MAX_GOAL_AUTO_RUNS
        ));
        return;
    }
    super::diagnostic_log(&format!(
        "[ki goal] auto-continue: {} (turns {})",
        goal.title, goal.turns_used
    ));
    if tx
        .send(TuiMessage::GoalContinue(goal.continuation_prompt()))
        .is_err()
    {
        super::diagnostic_log("warning: goal continuation dropped: the TUI channel is closed");
    }
}

pub(super) async fn drain_agent_events(
    mut rx: broadcast::Receiver<AgentEvent>,
    tui: Arc<TuiAltScreen>,
    state: Arc<TuiState>,
    chat: Arc<Container>,
    lane: Arc<dyn AgentLane>,
    harness: AgentHarness,
    mcp: Arc<kiqi_mcp::McpManager>,
    tx: mpsc::UnboundedSender<TuiMessage>,
    wake_state: Arc<crate::signal_host::SignalHost>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                handle_agent_event(
                    event,
                    &tui,
                    &state,
                    &chat,
                    &lane,
                    &harness,
                    &mcp,
                    &tx,
                    &wake_state,
                )
                .await
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // We dropped some intermediate deltas; the next MessageUpdate/
                // MessageEnd carries a full partial snapshot so the UI re-syncs.
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Apply a single `AgentEvent` to the UI. Mirrors the TS `handleEvent` switch
/// (`interactive-mode.ts:3068-3396`).
pub(super) async fn handle_agent_event(
    event: AgentEvent,
    tui: &Arc<TuiAltScreen>,
    state: &Arc<TuiState>,
    chat: &Arc<Container>,
    lane: &Arc<dyn AgentLane>,
    harness: &AgentHarness,
    mcp: &Arc<kiqi_mcp::McpManager>,
    tx: &mpsc::UnboundedSender<TuiMessage>,
    wake_state: &Arc<crate::signal_host::SignalHost>,
) {
    match event {
        AgentEvent::AgentStart => {
            state.set_status(RunStatus::Working);
            tui.request_render(false);
        }

        AgentEvent::AgentEnd { .. } => {
            // Finalize any still-streaming assistant message.
            state.transcript_view().finalize_assistant();
            state.set_status(RunStatus::Idle);
            // Flush any `!command` results that completed mid-run: the run's
            // tool sequence is closed, so appending now keeps transcript order
            // intact (upstream's `flushPendingBashMessages`).
            let pending: Vec<AgentMessage> =
                std::mem::take(&mut *state.pending_bash_messages.lock().unwrap());
            for message in pending {
                if let Err(error) = lane.append_message(message).await {
                    add_error_message(chat, &format!("Could not record bash result: {error}"));
                }
            }
            // Same boundary, same reason: MCP resource-update notices gathered
            // during the run append now, never mid-run. The record goes in
            // FIRST (I1); the wake decision only adds a short trigger after it.
            let woke = handle_signal_boundary(harness, mcp, wake_state, tx).await;
            if !woke {
                maybe_continue_goal(lane, wake_state, tx).await;
            }
            // The run drained the queue; refresh so consumed entries drop out
            // of the pending-messages display.
            refresh_pending_messages(state, lane).await;
            tui.request_render(false);
        }

        AgentEvent::RetryScheduled {
            attempt,
            max_retries,
            delay_ms,
            ..
        } => {
            state.show_retry(attempt, max_retries, delay_ms);
            tui.request_render(false);
        }

        AgentEvent::TurnStart => {
            // A new turn: reset the streaming-assistant guard so the next
            // MessageStart creates a fresh component.
            state.transcript_view().finalize_assistant();
        }

        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => {
            // Finalize the assistant message for this turn.
            let view = state.transcript_view();
            if let AgentMessage::Assistant(a) = &message {
                view.finalize_assistant_blocks(&assistant_blocks(a));
            } else {
                view.finalize_assistant();
            }
            // Tool results whose components were never ended by a
            // ToolExecutionEnd are handled by the normal path; there is nothing
            // extra to finalize here.
            let _ = tool_results;
            tui.request_render(false);
        }

        AgentEvent::MessageStart { message } => match message {
            AgentMessage::Assistant(a) => {
                // The shared transcript view renders this through the same
                // component path as the remote client. It installs the live
                // markdown transformer + hide-thinking preference itself.
                state.transcript_view().apply(
                    &UiEvent::AssistantStart {
                        blocks: assistant_blocks(&a),
                    },
                    tui.width(),
                );
                tui.request_render(false);
            }
            AgentMessage::Custom(custom) => {
                let payload = serde_json::json!({
                    "customType": custom.role,
                    "content": custom.content,
                    "details": custom.data,
                    "expanded": false,
                    "outputPad": 1,
                });
                if let Some(component) = extension_message_component(
                    &state.extension_session,
                    &custom.role,
                    &payload,
                    state.markdown_transformer(),
                ) {
                    chat.add_child(component);
                    chat.add_child(Arc::new(Spacer::new(1)));
                    tui.request_render(false);
                } else {
                    add_note_message(chat, &custom_message_fallback(&custom));
                    tui.request_render(false);
                }
            }
            // Queued (steering / follow-up) user messages are rendered here,
            // not at queue time, so a message that is still waiting never looks
            // delivered. The loop drains those itself and emits
            // `message_start` for them (`agent_loop.rs` pending-message drain).
            //
            // A DIRECTLY-sent prompt does NOT reach this arm: the harness
            // persists it up front and runs the loop with an empty prompts vec,
            // so no `message_start` is emitted for it — the submit handler
            // renders that bubble instead.
            AgentMessage::User(user) => {
                state.transcript_view().add_user(&user_message_text(&user));
                add_user_images(&state.chat_container, &user, state.images_visible());
                // A consumed entry must drop out of the pending display.
                refresh_pending_messages(state, lane).await;
                tui.request_render(false);
            }
            // ToolResult / other starts are echoed via the tool-execution
            // components; ignore the dupes.
            _ => {}
        },

        AgentEvent::MessageUpdate {
            message,
            assistant_message_event,
        } => {
            let _ = assistant_message_event; // the snapshot is authoritative
            let a: &kiqi_ai::types::AssistantMessage = &message;
            let text = assistant_text(a);
            // The shared view creates/updates tool + bash panels from the
            // snapshot's finalized tool calls and streams the block list, so a
            // thinking block renders live exactly as the remote client does.
            state.transcript_view().apply(
                &UiEvent::AssistantUpdate {
                    blocks: assistant_blocks(a),
                    tool_calls: assistant_tool_calls(a),
                },
                tui.width(),
            );
            // A freshly-created bash panel hides the global `Working…` loader.
            state.sync_working_loader_with_bash();
            *state.last_assistant_text.lock().unwrap() = text;
            tui.request_render(false);
        }

        AgentEvent::MessageEnd { message } => {
            if let AgentMessage::Assistant(a) = &message {
                let text = assistant_text(a);
                state
                    .transcript_view()
                    .finalize_assistant_blocks(&assistant_blocks(a));
                // Cache the finalized text for `/copy`.
                if !text.is_empty() {
                    *state.last_assistant_text.lock().unwrap() = text;
                }
                // Cache-miss notice (`maybeShowCacheMissNotice`): the previous
                // turn's input established a cacheable prefix; a prompt that
                // re-reads less than it should means the prefix was re-billed.
                // Uses the shared cache-stats logic (noise floor, idle gap,
                // model change) and prices the miss when known.
                let usage = &a.usage;
                // Footer usage totals + cache hit rate (pi footer.ts).
                state.footer.add_usage(
                    usage.input,
                    usage.output,
                    usage.cache_read,
                    usage.cache_write,
                    usage.cost.total,
                );
                let denom = usage.input + usage.cache_read + usage.cache_write;
                if denom > 0 && (usage.cache_read + usage.cache_write) > 0 {
                    state
                        .footer
                        .set_cache_hit_rate(Some(usage.cache_read as f64 / denom as f64 * 100.0));
                }
                // Context-window badge (`?/512k` → `63.2%/512k`): the provider
                // reports the prompt token count for this request, which is the
                // context size the footer displays.
                record_context_usage(&state.footer, a);
                let miss = state
                    .cache_tracker
                    .lock()
                    .unwrap()
                    .observe(a, &kiqi_harness::cache_stats::NoPrices);
                // upstream gates these behind `showCacheMissNotices`
                // (default `false`); the tracker still runs so the footer's
                // cache-hit rate stays accurate either way.
                let cache_miss_notices = *state.cache_miss_notices.lock().unwrap();
                if let Some(miss) = miss.filter(|_| cache_miss_notices) {
                    let cost = if miss.missed_cost > 0.0 {
                        format!(" (~${:.4})", miss.missed_cost)
                    } else {
                        String::new()
                    };
                    add_note_message(
                        &state.chat_container,
                        &format!(
                            "Cache miss: {} tokens re-billed{}",
                            format_tokens(miss.missed_tokens),
                            cost
                        ),
                    );
                }
                if let Some(text) = extension_usage_text(Some(&state.extension_session), usage) {
                    add_note_message(chat, &text);
                }
                // Error assistant messages carry the provider diagnostic in
                // `error_message`, not in text content. The assistant
                // component is empty for these messages, so surface the
                // diagnostic as a visible error row in the transcript.
                if let Some(error) = assistant_error_text(a) {
                    add_error_message(chat, &error);
                }
            }
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            state.transcript_view().apply(
                &UiEvent::ToolStart {
                    id: tool_call_id,
                    name: tool_name,
                    args,
                },
                tui.width(),
            );
            state.sync_working_loader_with_bash();
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => {
            state.transcript_view().apply(
                &UiEvent::ToolUpdate {
                    id: tool_call_id,
                    name: tool_name,
                    args,
                    result: crate::remote::protocol::tool_result_json(&partial_result),
                },
                tui.width(),
            );
            state.sync_working_loader_with_bash();
            tui.request_render(false);
        }

        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            state.transcript_view().apply(
                &UiEvent::ToolEnd {
                    id: tool_call_id,
                    name: tool_name,
                    result: crate::remote::protocol::tool_result_json(&result),
                    is_error,
                },
                tui.width(),
            );
            add_tool_images(
                &state.chat_container,
                &result.content,
                state.images_visible(),
            );
            state.sync_working_loader_with_bash();
            // Bash tool completion is user-visible: immediate render ensures the
            // result is displayed even when the scheduler thread is busy or the
            // throttle window has not elapsed.
            tui.render_now(false);
        }
    }
}

/// Update the footer's context-window badge from one assistant response.
///
/// `Usage::context_tokens()` is the prompt size the provider reported for that
/// request — the value pi's footer renders as `{percent}%/{window}`. Aborted
/// and errored turns are skipped (their usage is not a real context
/// measurement, mirroring pi's `lastAssistantUsageInfo`) so the last valid
/// count stays on screen instead of flickering to `?`.
pub(super) fn record_context_usage(
    footer: &kiqi_tui::FooterComponent,
    message: &kiqi_ai::AssistantMessage,
) {
    if message.stop_reason == kiqi_ai::StopReason::Aborted
        || message.stop_reason == kiqi_ai::StopReason::Error
    {
        return;
    }
    let tokens = message.usage.context_tokens();
    if tokens > 0 {
        footer.set_context_tokens(Some(tokens));
    }
}

/// Return the diagnostic carried by a failed or aborted provider request.
/// Providers may omit `error_message`; keep a stable fallback so a terminal
/// request failure can never render as an empty transcript turn.
pub(super) fn assistant_error_text(message: &kiqi_ai::AssistantMessage) -> Option<String> {
    let fallback = match message.stop_reason {
        kiqi_ai::StopReason::Error => "Provider request failed.",
        kiqi_ai::StopReason::Aborted => "Request aborted.",
        _ => return None,
    };
    Some(
        message
            .error_message
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(fallback)
            .to_string(),
    )
}

/// Whether a tool result opts into markdown rendering of its body.
///
/// A tool signals this by setting `details.markdown = true` on its result
/// (the `kiqi-todo` extension does this for its progress bar + table payload).
/// The host then hands the body to the markdown renderer instead of printing
/// it as plain text — see `ToolExecutionComponent::set_result_markdown`.
/// Absent or non-`true` details keep the existing plain-text rendering, so
/// every other tool is unaffected.
pub(super) fn tool_result_requests_markdown(details: Option<&serde_json::Value>) -> bool {
    details
        .and_then(|details| details.get("markdown"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

/// What the server said about who wrote the changed entries, for the skip line.
///
/// The echo decision is `authored_sessions` vs the session ki serves, and the two
/// ways it can decline to suppress look identical in a log that only prints the
/// reason string: a server that reports no provenance at all (`_meta.sessions`
/// absent) and a server whose reported sessions differ from ours both end at
/// "recorded without wake". Distinguishing them took an external probe the first
/// time it mattered, so the batch now records which it was.
///
/// Counts and a boolean, not the ids: the ids of a peer's sessions are not this
/// session's business, and a long batch would bloat the log.
fn provenance_summary(batch: &kiqi_mcp::McpNoticeBatch) -> String {
    let attributed = batch
        .notices
        .iter()
        .filter(|notice| !notice.authored_sessions.is_empty())
        .count();
    format!("attributed={attributed}/{}", batch.notices.len())
}

#[cfg(test)]
mod tests {
    use super::{done_when_cmd_satisfied, provenance_summary};

    /// The summary must tell "the server sent no `_meta.sessions`" apart from
    /// "it sent sessions that are not ours" — both skip with the same reason.
    #[test]
    fn the_skip_line_records_whether_the_server_attributed_anything() {
        let notice = |authored: Vec<&str>| kiqi_mcp::McpNotice {
            server: "walgit".into(),
            uri: "walgit://refs/o/r".into(),
            content: None,
            authored_sessions: authored.into_iter().map(str::to_string).collect(),
        };
        let batch = |notices: Vec<kiqi_mcp::McpNotice>| kiqi_mcp::McpNoticeBatch {
            notices,
            suppressed: 0,
        };

        // The state a server without `_meta.sessions` produces.
        assert_eq!(
            provenance_summary(&batch(vec![notice(Vec::new())])),
            "attributed=0/1"
        );
        // Attributed, so the comparison (not the absence) decided.
        assert_eq!(
            provenance_summary(&batch(vec![notice(vec!["peer"])])),
            "attributed=1/1"
        );
        // A mixed batch reports the fraction, not a single verdict.
        assert_eq!(
            provenance_summary(&batch(vec![notice(vec!["peer"]), notice(Vec::new())])),
            "attributed=1/2"
        );
    }

    #[tokio::test]
    async fn a_zero_exit_done_command_is_done_a_nonzero_exit_is_not() {
        let dir = tempfile::tempdir().unwrap();
        assert!(done_when_cmd_satisfied(dir.path(), "true").await);
        assert!(!done_when_cmd_satisfied(dir.path(), "false").await);
        assert!(!done_when_cmd_satisfied(dir.path(), "exit 3").await);
    }

    #[tokio::test]
    async fn the_done_command_runs_in_the_session_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "x").unwrap();
        assert!(done_when_cmd_satisfied(dir.path(), "test -f marker").await);
        assert!(!done_when_cmd_satisfied(dir.path(), "test -f missing").await);
    }
}
