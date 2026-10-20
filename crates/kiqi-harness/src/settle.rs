//! Commit-on-settle: persist a run's messages when they settle, not at run end.
//!
//! # Why this exists
//!
//! `tool_started` is the record that lets recovery say *which tool's side effect
//! is unknown* after a crash. It cannot be written from ki's old model, where a
//! whole run's messages were persisted in one pass at the end: the reducer's
//! `validate_tool_start` requires `assistant_entry_id` to **already exist** with
//! a matching tool call at `tool_index`, and a tool runs long before the run
//! finishes. Upstream satisfies that requirement by committing the assistant
//! message when the step settles (`assistant.ready`), *then* running its tools.
//!
//! This module ports that ordering: an assistant message carrying tool calls is
//! committed as soon as it is final (`MessageEnd`), with a **reserved** entry id
//! that the matching `step_attempt` record names. Once the message is an entry,
//! its tools can be recorded before they run.
//!
//! # What is deliberately *not* committed early
//!
//! - **The final assistant message** (no tool calls): it ends the run and is
//!   persisted by the normal run-end pass, so `leaf_id`/`final_entry_id` and the
//!   `Completed` outcome keep their existing meaning.
//! - **A message from an attempt that will be retried**: the harness re-runs the
//!   whole loop on a retryable failure, so committing a failed attempt's message
//!   would leave a dead assistant message in history and put two assistant turns
//!   in a row in front of the provider. The retry decision is computable at
//!   settle time (`enabled && attempt < max && is_retryable_error`), and is
//!   evaluated here with the same inputs the retry loop uses.
//!
//! # Interaction with the compaction cut
//!
//! A message committed here is already an entry, so the run-end pass cannot
//! "skip" it via `post_compaction_cut`. That means messages summarized by a
//! mid-run compaction stay in the log. That matches upstream (its compaction
//! commits a summary entry and leaves the pre-summary entries in place; context
//! building starts after the last compaction via the cut-point scan) and is the
//! reason this is a deliberate trade rather than an oversight.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use kiqi_agent::{AgentEmitter, AgentEvent, AgentMessage};
use kiqi_ai::types::{AssistantMessage, StopReason};

use crate::agent_harness::AgentHarness;
use crate::session::types::{
    ProvisionedEntry, ProvisionedKind, RecordBase, StepKind, ToolReplay, ToolStartedRecord,
};

/// Whether a settled assistant message is part of the run's final history.
/// Mirrors the retry loop's own eligibility test so the two cannot disagree
/// about which attempt survives.
pub fn will_retry(
    message: &AssistantMessage,
    retry_enabled: bool,
    attempt: u32,
    max_retries: u32,
) -> bool {
    retry_enabled
        && attempt < max_retries
        && message.stop_reason == StopReason::Error
        && crate::compaction::is_retryable_assistant_error(message)
}

/// Whether a settled assistant message must be committed early.
///
/// Only messages that lead into tool execution need to be entries before the run
/// ends, because only they are referenced by `tool_started` records.
fn needs_early_commit(message: &AssistantMessage, retrying: bool) -> bool {
    !retrying
        && message
            .content
            .iter()
            .any(|block| matches!(block, kiqi_ai::types::Content::ToolCall(_)))
}

/// What a settle observer needs to know about the run it is watching. Shared
/// with the run loop, which owns the retry counter.
pub struct SettleState {
    /// Index of the next loop message, in `new_messages` order. Advanced by
    /// `MessageStart` so a commit can name the index it persisted.
    seen: AtomicUsize,
    /// The attempt number the retry loop is currently on.
    attempt: AtomicU32,
    retry_enabled: bool,
    max_retries: u32,
    run_id: String,
    /// Per-tool replay policy, from the run's tool snapshot. A `ToolStarted`
    /// record has to carry it, and the settle observer sees only the event.
    tool_replays: BTreeMap<String, ToolReplay>,
    /// Next assistant stream index. Mirrors the frame recorder, which numbers
    /// **every** assistant message, including text-only ones that never settle
    /// early; counting settled messages instead retires the wrong stream.
    assistant_stream: AtomicUsize,
    /// The stream currently streaming, if any.
    current_stream: Mutex<Option<usize>>,
    /// An assistant `Start` happened since the last `set_attempt`. A retry
    /// discards that attempt's message, so `seen` must be rolled back with it.
    attempt_started: AtomicBool,
    /// Indices already committed, so the run-end pass does not duplicate them.
    committed: Mutex<BTreeSet<usize>>,
    /// The assistant entry each settled message produced, by index.
    assistant_entries: Mutex<BTreeMap<usize, String>>,
    /// `tool_call_id` → (assistant entry id, ordinal within that message). Filled
    /// when a message carrying tool calls settles, read when each tool starts.
    pending_calls: Mutex<BTreeMap<String, (String, u32)>>,
    /// `tool_call_id` → reserved result entry id, so the tool result is committed
    /// under the id its `tool_started` record already named.
    reserved_results: Mutex<BTreeMap<String, String>>,
    /// Queued messages handed to the loop for injection, in the order they will be
    /// emitted. Injected user messages are *not* committed by the run-end pass
    /// until the whole run finishes, so without committing them at their own
    /// settle a crash mid-run would lose the text the model was already given —
    /// while the queue item was already cancelled, making it unrecoverable.
    injected: Mutex<VecDeque<(String, AgentMessage)>>,
    /// The terminal message of the last **soft-interrupted** provider request.
    /// The loop drops it from `new_messages`, but it is still the newest thing
    /// the run produced and the honest `final_message` to report.
    soft_tail: Mutex<Option<AssistantMessage>>,
    /// True while `soft_tail` is the newest event: cleared by the next
    /// `MessageStart`, so a later reply (a steering-driven one, say) wins over
    /// the stale tail.
    soft_tail_pending: AtomicBool,
    /// The `MessageEnd` the loop emits immediately after a *retracted* soft
    /// message belongs to that message and must neither be index-assigned nor
    /// committed.
    skip_next_assistant_end: AtomicBool,
}

impl SettleState {
    pub fn new(
        run_id: impl Into<String>,
        retry_enabled: bool,
        max_retries: u32,
        tool_replays: BTreeMap<String, ToolReplay>,
    ) -> Self {
        Self {
            seen: AtomicUsize::new(0),
            attempt: AtomicU32::new(0),
            retry_enabled,
            max_retries,
            run_id: run_id.into(),
            tool_replays,
            assistant_stream: AtomicUsize::new(0),
            current_stream: Mutex::new(None),
            attempt_started: AtomicBool::new(false),
            committed: Mutex::new(BTreeSet::new()),
            assistant_entries: Mutex::new(BTreeMap::new()),
            pending_calls: Mutex::new(BTreeMap::new()),
            reserved_results: Mutex::new(BTreeMap::new()),
            injected: Mutex::new(VecDeque::new()),
            soft_tail: Mutex::new(None),
            soft_tail_pending: AtomicBool::new(false),
            skip_next_assistant_end: AtomicBool::new(false),
        }
    }

    /// Record a soft-interrupted request whose assistant message the loop
    /// dropped. Called from [`AgentEmitter::request_soft_interrupted`], i.e.
    /// synchronously, before the loop emits that message's `MessageEnd`.
    ///
    /// `announced` means the loop emitted `MessageStart` for it: `seen` was
    /// advanced for a message that never enters `new_messages`, so retract it,
    /// and arm the latch that swallows the following `MessageEnd`.
    pub fn note_soft_interrupted(
        &self,
        message: AssistantMessage,
        announced: bool,
    ) -> Option<usize> {
        *self.soft_tail.lock().unwrap() = Some(message);
        self.soft_tail_pending.store(true, Ordering::Release);
        if !announced {
            return None;
        }
        // The stream the dropped message created. The caller retires it so a
        // crash cannot salvage the partial back into history.
        let stream = self.current_stream.lock().unwrap().take();
        // Saturating: a provider that restarts a stream without a terminal
        // event must not make this underflow.
        let _ = self
            .seen
            .try_update(Ordering::AcqRel, Ordering::Acquire, |seen| {
                Some(seen.saturating_sub(1))
            });
        self.skip_next_assistant_end.store(true, Ordering::Release);
        // The retraction above already removed this Start; a later retry must
        // not roll it back a second time.
        self.attempt_started.store(false, Ordering::Release);
        stream
    }

    /// The last soft-interrupted message, if it is still the newest event.
    ///
    /// `None` once a later `MessageStart` arrived: that message is history now.
    pub fn take_soft_tail(&self) -> Option<AssistantMessage> {
        if !self.soft_tail_pending.swap(false, Ordering::AcqRel) {
            return None;
        }
        self.soft_tail.lock().unwrap().take()
    }

    /// Register a message the run is about to inject, with the entry id its
    /// `queue_enqueued` record reserved for it.
    pub fn push_injected(&self, entry_id: impl Into<String>, message: AgentMessage) {
        self.injected
            .lock()
            .unwrap()
            .push_back((entry_id.into(), message));
    }

    /// Advance the attempt counter; called by the retry loop so the settle
    /// observer sees the same number the loop's next attempt will use.
    pub fn set_attempt(&self, attempt: u32) {
        // A retried attempt discarded its partial assistant message, but its
        // `MessageStart` already advanced `seen`. Roll that one event back so
        // the accepted attempt's commit indices stay in `new_messages` space.
        // Committed indices are deliberately left alone: a retry only happens
        // before a message is committed (`from_assistant` disables retries once
        // tools ran).
        if self.attempt_started.swap(false, Ordering::AcqRel) {
            let _ = self
                .seen
                .try_update(Ordering::AcqRel, Ordering::Acquire, |seen| {
                    Some(seen.saturating_sub(1))
                });
        }
        self.attempt.store(attempt, Ordering::Release);
    }

    /// Indices already persisted by this observer.
    pub fn committed_indices(&self) -> BTreeSet<usize> {
        self.committed.lock().unwrap().clone()
    }

    pub fn committed_count(&self) -> usize {
        self.committed.lock().unwrap().len()
    }

    fn record_commit(&self, index: usize, entry_id: &str) {
        self.committed.lock().unwrap().insert(index);
        self.assistant_entries
            .lock()
            .unwrap()
            .insert(index, entry_id.to_string());
    }

    /// Remember each tool call of a freshly committed assistant message, keyed so
    /// `tool_execution_start` can record it against the right entry and ordinal.
    fn record_tool_calls(&self, entry_id: &str, message: &AssistantMessage) {
        let mut pending = self.pending_calls.lock().unwrap();
        for (ordinal, block) in message.content.iter().enumerate() {
            if let kiqi_ai::types::Content::ToolCall(call) = block {
                pending.insert(call.id.clone(), (entry_id.to_string(), ordinal as u32));
            }
        }
    }
}

/// Emitter wrapper that commits settled messages to the session.
///
/// Sits *around* the frame recorder, so frames are still appended for every
/// delta before the message is committed — a crash between "settled" and
/// "committed" is then still recoverable from the frames.
pub struct SettlingEmitter {
    inner: Arc<dyn AgentEmitter>,
    /// The frame recorder, kept as a concrete handle so a committed message can
    /// retire its own stream. Forwarding still goes through `inner`.
    recorder: Arc<crate::frame_progress::FrameRecordingEmitter>,
    state: Arc<SettleState>,
    harness: AgentHarness,
}

impl SettlingEmitter {
    pub fn new(
        inner: Arc<dyn AgentEmitter>,
        recorder: Arc<crate::frame_progress::FrameRecordingEmitter>,
        state: Arc<SettleState>,
        harness: AgentHarness,
    ) -> Self {
        Self {
            inner,
            recorder,
            state,
            harness,
        }
    }

    /// Commit one settled assistant message and record its `step_attempt`.
    async fn settle_assistant(
        &self,
        index: usize,
        stream_index: Option<usize>,
        message: &AssistantMessage,
    ) {
        let entry = ProvisionedEntry {
            id: self.harness.next_entry_id(),
            kind: ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(message.clone())),
                terminate: None,
            },
        };
        let entry_id = entry.id.clone();
        match self.harness.append_provisioned(entry).await {
            Ok(_) => {
                self.state.record_commit(index, &entry_id);
                self.state.record_tool_calls(&entry_id, message);
                // The message is an entry now, so its frames are no longer
                // progress. Retiring the stream is what stops a crash in a *later*
                // turn from replaying this one and duplicating it.
                if let Some(stream_index) = stream_index {
                    if let Err(error) = self.recorder.clear_stream(stream_index).await {
                        tracing::warn!(
                            stream_index,
                            %error,
                            "could not retire a committed assistant stream"
                        );
                    }
                }
                // One assistant generation = one step attempt. The entry id is
                // the one just committed, so recovery can check whether the step
                // actually landed.
                if let Err(error) = self
                    .harness
                    .write_step_attempt_record(
                        &self.state.run_id,
                        StepKind::Assistant,
                        self.state.attempt.load(Ordering::Acquire) + 1,
                        &entry_id,
                        None,
                    )
                    .await
                {
                    tracing::warn!(%error, "could not record a settled assistant step");
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not commit a settled assistant message");
            }
        }
    }

    /// Record a tool call that is about to run, and reserve the id its result
    /// will be committed under.
    ///
    /// Skipped with a warning when the call's assistant message was never
    /// committed: a `tool_started` record must reference an existing assistant
    /// entry (the reducer rejects a mismatch), so silently writing one would
    /// corrupt the log.
    async fn start_tool(&self, tool_call_id: &str, tool_name: &str, args: &serde_json::Value) {
        let Some((assistant_entry_id, tool_index)) = self
            .state
            .pending_calls
            .lock()
            .unwrap()
            .get(tool_call_id)
            .cloned()
        else {
            tracing::warn!(
                tool_call_id,
                "skipping a tool_started record: the assistant message carrying this call was not committed"
            );
            return;
        };
        let result_entry_id = self.harness.next_entry_id();
        self.state
            .reserved_results
            .lock()
            .unwrap()
            .insert(tool_call_id.to_string(), result_entry_id.clone());
        let replay = self
            .state
            .tool_replays
            .get(tool_name)
            .copied()
            .unwrap_or(ToolReplay::Never);
        let record = ToolStartedRecord {
            base: RecordBase {
                id: self.harness.next_entry_id(),
                seq: 0,
                // The store re-stamps the lane; it must still name a real one,
                // since the lane is how the store routes the append.
                lane: self.harness.lane_name().to_string(),
                timestamp: 0,
            },
            run_id: self.state.run_id.clone(),
            assistant_entry_id,
            tool_index,
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            effective_args: args.clone(),
            result_entry_id,
            replay,
        };
        if let Err(error) = self.harness.write_tool_started(record).await {
            tracing::warn!(%error, tool_call_id, "could not record a tool_started frame");
        }
    }

    /// Commit an injected user message under the id its queue record reserved.
    ///
    /// Injected messages are the only user messages that appear inside the loop
    /// (`run_core` drives it with an empty prompt list and persists the caller's
    /// prompts itself), so the queue of registered injections lines up with the
    /// `MessageEnd` events in order. The content is compared before committing: if
    /// they ever diverge, committing the wrong text under that id would be worse
    /// than dropping the optimisation.
    async fn settle_injected(&self, index: usize, message: &AgentMessage) {
        let pending = {
            let mut injected = self.state.injected.lock().unwrap();
            match injected.front() {
                Some((_, queued)) if queued == message => injected.pop_front(),
                _ => None,
            }
        };
        let Some((entry_id, _)) = pending else {
            return;
        };
        let entry = ProvisionedEntry {
            id: entry_id,
            kind: ProvisionedKind::Message {
                message: message.clone(),
                terminate: None,
            },
        };
        match self.harness.append_provisioned(entry).await {
            Ok(_) => {
                self.state.committed.lock().unwrap().insert(index);
            }
            Err(error) => {
                tracing::warn!(%error, "could not commit an injected message");
            }
        }
    }

    /// Commit a settled tool result under the id its `tool_started` named.
    async fn settle_tool_result(&self, index: usize, message: &kiqi_ai::types::ToolResultMessage) {
        let Some(result_entry_id) = self
            .state
            .reserved_results
            .lock()
            .unwrap()
            .get(&message.tool_call_id)
            .cloned()
        else {
            // No reserved id (e.g. the call was never recorded): fall back to the
            // run-end pass, which mints an id normally.
            return;
        };
        let entry = ProvisionedEntry {
            id: result_entry_id,
            kind: ProvisionedKind::Message {
                message: AgentMessage::ToolResult(Box::new(message.clone())),
                terminate: None,
            },
        };
        match self.harness.append_provisioned(entry).await {
            Ok(_) => {
                self.state.committed.lock().unwrap().insert(index);
            }
            Err(error) => {
                tracing::warn!(%error, "could not commit a settled tool result");
            }
        }
    }

    fn handle(&self, event: &AgentEvent) -> Option<BoxFuture<'static, ()>> {
        match event {
            AgentEvent::MessageStart { message } => {
                // A later message makes a previous soft tail stale: it is no
                // longer the newest thing the run produced.
                self.state.soft_tail_pending.store(false, Ordering::Release);
                self.state.seen.fetch_add(1, Ordering::AcqRel);
                if matches!(message, AgentMessage::Assistant(_)) {
                    let stream = self.state.assistant_stream.fetch_add(1, Ordering::AcqRel);
                    *self.state.current_stream.lock().unwrap() = Some(stream);
                    self.state.attempt_started.store(true, Ordering::Release);
                }
                None
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(assistant),
            } => {
                // The retracted soft-interrupted message. Its `MessageStart` was
                // undone by `note_soft_interrupted`, and it is not in
                // `new_messages`, so there is nothing to index or commit — in
                // particular, a tool call it happened to contain must not be
                // committed early or get a `step_attempt`.
                if self
                    .state
                    .skip_next_assistant_end
                    .swap(false, Ordering::AcqRel)
                {
                    return None;
                }
                // `seen` was advanced by this message's `MessageStart`; the index
                // of the message that just settled is one less.
                let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
                let retrying = will_retry(
                    assistant,
                    self.state.retry_enabled,
                    self.state.attempt.load(Ordering::Acquire),
                    self.state.max_retries,
                );
                if !needs_early_commit(assistant, retrying) {
                    return None;
                }
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                let stream_index = self.state.current_stream.lock().unwrap().take();
                let message = (**assistant).clone();
                Some(Box::pin(async move {
                    emitter
                        .settle_assistant(index, stream_index, &message)
                        .await;
                }))
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::ToolResult(result),
            } => {
                let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                let result = (**result).clone();
                Some(Box::pin(async move {
                    emitter.settle_tool_result(index, &result).await;
                }))
            }
            AgentEvent::MessageEnd {
                message: message @ AgentMessage::User(_),
            } => {
                let index = self.state.seen.load(Ordering::Acquire).saturating_sub(1);
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                let message = message.clone();
                Some(Box::pin(async move {
                    emitter.settle_injected(index, &message).await;
                }))
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                let emitter = SettlingEmitter {
                    inner: Arc::clone(&self.inner),
                    recorder: Arc::clone(&self.recorder),
                    state: Arc::clone(&self.state),
                    harness: self.harness.clone(),
                };
                let tool_call_id = tool_call_id.clone();
                let tool_name = tool_name.clone();
                let args = args.clone();
                Some(Box::pin(async move {
                    emitter.start_tool(&tool_call_id, &tool_name, &args).await;
                }))
            }
            _ => None,
        }
    }
}

impl AgentEmitter for SettlingEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let inner = Arc::clone(&self.inner);
        let settles = self.handle(&event);
        Box::pin(async move {
            // Commit *before* forwarding: a UI that renders the settled message
            // must not be able to observe it before it is durable.
            if let Some(settles) = settles {
                settles.await;
            }
            inner.emit(event).await;
        })
    }

    fn try_emit(&self, event: AgentEvent) {
        self.inner.try_emit(event);
    }

    fn request_soft_interrupted(
        &self,
        message: AssistantMessage,
        announced: bool,
    ) -> BoxFuture<'static, ()> {
        // Apply the retraction synchronously: the loop emits the matching
        // `MessageEnd` right after awaiting this future, and the skip latch must
        // already be armed by then. The returned index names the frame stream
        // the dropped message created, so it can be retired before a crash
        // salvages it back into history.
        let stream = self.state.note_soft_interrupted(message.clone(), announced);
        let recorder = Arc::clone(&self.recorder);
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            if let Some(stream_index) = stream {
                if let Err(error) = recorder.clear_stream(stream_index).await {
                    tracing::warn!(
                        stream_index,
                        %error,
                        "could not retire a soft-interrupted assistant stream"
                    );
                }
            }
            // Forward like `try_emit`: a decorator must not swallow a signal
            // meant for an emitter further in.
            inner.request_soft_interrupted(message, announced).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::Ordering;

    #[test]
    fn a_retry_rolls_back_the_failed_attempts_assistant_start() {
        let state = SettleState::new("run", true, 3, BTreeMap::new());
        // The failed attempt announced an assistant message that the retry
        // discarded from `new_messages`.
        state.seen.fetch_add(1, Ordering::AcqRel);
        state.attempt_started.store(true, Ordering::Release);
        state.set_attempt(1);
        assert_eq!(state.seen.load(Ordering::Acquire), 0);
        assert!(!state.attempt_started.load(Ordering::Acquire));

        // A retry with no assistant start rolls back nothing.
        state.seen.store(2, Ordering::Release);
        state.set_attempt(2);
        assert_eq!(state.seen.load(Ordering::Acquire), 2);
    }
}
