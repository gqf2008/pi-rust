//! Soft interrupt (phase 1: tool calls only, `docs/design/soft-interrupt.md`).
//!
//! The invariant that matters and that a single-tool test cannot see: after one
//! soft interrupt, the *rest of the run* must keep working. `install()` cancels
//! the token it is handed whenever the soft flag is latched, so the flag has to
//! be drained at the action boundary where it took effect — otherwise every
//! later tool call in the same run starts out cancelled and the loop spends the
//! remaining turns collecting cancelled results.
//!
//! `a_soft_interrupt_does_not_cancel_later_tool_calls` pins exactly that: delete
//! the `take_soft()` in `run_one_tool_call` and the "clean" count drops to 0
//! while the run still finishes, which is the bug in its full shape.
//!
//! Phase 2 extends the same rule to the *model stream* (`a_stream_soft_interrupt_*`).
//! A soft-interrupted request must leave no trace: the partial message is popped
//! from the context, nothing is appended to `new_messages` (the only vec the
//! harness persists), and the run signal stays live so the next prompt continues
//! the conversation instead of hitting a cancelled run.
//!
//! ```text
//! cancel the run signal        -> hard abort: keep the Aborted assistant, end the run
//! cancel the per-request child -> soft interrupt: drop the attempt, keep the run
//! ```
//!
//! Every phase-2 test fakes the provider contract real providers implement: a
//! cancelled request stops producing deltas and reports `StopReason::Aborted`.

#[path = "common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::assistant_tool_calls;
use kiqi_agent::interrupt::InterruptControl;
use kiqi_agent::{AgentContext, AgentEvent, AgentMessage, AgentTool, AgentToolResult};
use kiqi_ai::types::{
    AssistantMessageEvent, Content, DoneReason, ErrorReason, Message, StopReason, Tool,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Counts executions, and executions whose token arrived *not* already
/// cancelled.
///
/// The soft interrupt hits the first call only, so a healthy run reports
/// `clean == total - 1`. An undrained soft flag reports `clean == 0`.
///
/// The first call blocks until its token is cancelled (that is what gives the
/// interrupter a window to observe `has_current()`); later calls return at once.
struct CountingTool {
    schema: Tool,
    total: Arc<AtomicUsize>,
    clean: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentTool for CountingTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "Counting"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(kiqi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, kiqi_agent::AgentError> {
        let index = self.total.fetch_add(1, Ordering::SeqCst);
        if signal.is_cancelled() {
            // Already cancelled on arrival: the soft interrupt hit this call.
            return Ok(AgentToolResult::text("cancelled"));
        }
        if index == 0 {
            // The action the test interrupts: stay installed until the token
            // fires. The timeout is a safety net so a broken interrupt fails
            // the assertion instead of hanging the suite.
            let _ = tokio::time::timeout(Duration::from_secs(10), signal.cancelled()).await;
            if signal.is_cancelled() {
                return Ok(AgentToolResult::text("cancelled"));
            }
        }
        self.clean.fetch_add(1, Ordering::SeqCst);
        Ok(AgentToolResult::text("ran"))
    }
}

fn empty_schema(name: &str) -> Tool {
    Tool {
        name: name.to_string(),
        description: "Counting tool".to_string(),
        parameters: kiqi_ai::types::Schema::new(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })),
        constrained_sampling: None,
    }
}

#[tokio::test]
async fn a_soft_interrupt_does_not_cancel_later_tool_calls() {
    let interrupt = Arc::new(InterruptControl::new());
    let total = Arc::new(AtomicUsize::new(0));
    let clean = Arc::new(AtomicUsize::new(0));

    // Three tool-call turns, then a final text answer: enough turns that a
    // latched flag is visible (turn 2 and 3 would also be cancelled).
    let mut scripted: Vec<kiqi_ai::types::AssistantMessage> = (0..3)
        .map(|turn| {
            assistant_tool_calls(
                vec![(&format!("call-{turn}"), "counting", json!({}))],
                StopReason::ToolUse,
            )
        })
        .collect();
    scripted.push(common::assistant_text("done", StopReason::Stop));

    let mut config = common::base_config();
    config.interrupt = Arc::clone(&interrupt);
    config.tool_execution = kiqi_agent::ToolExecutionMode::Sequential;
    let handle = kiqi_agent::AbortHandle::new();
    config.signal = handle.token();

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(CountingTool {
            schema: empty_schema("counting"),
            total: Arc::clone(&total),
            clean: Arc::clone(&clean),
        })],
    };
    let stream_fn = common::mock_stream_fn(scripted);
    let (collector, events_buf) = kiqi_agent::CollectorEmitter::new();
    let emit: Arc<dyn kiqi_agent::AgentEmitter> = Arc::new(collector);

    // Fire the soft interrupt as soon as the first tool call is installed.
    let interrupter = {
        let interrupt = Arc::clone(&interrupt);
        tokio::spawn(async move {
            for _ in 0..5_000 {
                if interrupt.has_current() {
                    interrupt.soft_interrupt();
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            false
        })
    };

    let run = kiqi_agent::run_agent_loop(Vec::new(), context, config, emit, stream_fn).await;

    assert!(
        interrupter.await.expect("interrupter task"),
        "the first tool call was never installed"
    );
    let events = events_buf.lock().expect("events lock").clone();
    assert!(run.is_ok(), "the run survives a soft interrupt: {run:?}");

    let ran = total.load(Ordering::SeqCst);
    let clean_runs = clean.load(Ordering::SeqCst);
    assert_eq!(ran, 3, "every scripted tool call ran: {events:?}");
    assert_eq!(
        clean_runs, 2,
        "only the interrupted call sees a cancelled token; a latched soft flag \
         would cancel all three (ran={ran}, clean={clean_runs})"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::AgentEnd { .. })),
        "the run reached AgentEnd: {events:?}"
    );
    assert!(
        !handle.token().is_cancelled(),
        "the run signal was never cancelled — that is what makes this soft"
    );
}

/// A hard abort still ends the run (`LoopOutcome::Aborted`), i.e. the drain
/// added for soft interrupts did not turn abort into a soft one.
#[tokio::test]
async fn a_hard_abort_still_ends_the_run() {
    let interrupt = Arc::new(InterruptControl::new());
    let total = Arc::new(AtomicUsize::new(0));
    let clean = Arc::new(AtomicUsize::new(0));

    let scripted: Vec<kiqi_ai::types::AssistantMessage> = (0..3)
        .map(|turn| {
            assistant_tool_calls(
                vec![(&format!("call-{turn}"), "counting", json!({}))],
                StopReason::ToolUse,
            )
        })
        .collect();

    let mut config = common::base_config();
    config.interrupt = Arc::clone(&interrupt);
    config.tool_execution = kiqi_agent::ToolExecutionMode::Sequential;
    let handle = kiqi_agent::AbortHandle::new();
    config.signal = handle.token();

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(CountingTool {
            schema: empty_schema("counting"),
            total: Arc::clone(&total),
            clean: Arc::clone(&clean),
        })],
    };
    let stream_fn = common::mock_stream_fn(scripted);
    let (collector, events_buf) = kiqi_agent::CollectorEmitter::new();
    let emit: Arc<dyn kiqi_agent::AgentEmitter> = Arc::new(collector);

    let aborter = {
        let handle = handle.clone();
        tokio::spawn(async move {
            for _ in 0..5_000 {
                if handle.token().is_cancelled() {
                    return;
                }
                // Abort once the first tool started.
                tokio::time::sleep(Duration::from_millis(1)).await;
                handle.abort();
                return;
            }
        })
    };
    let _ = aborter;

    let run = kiqi_agent::run_agent_loop(Vec::new(), context, config, emit, stream_fn).await;
    let events = events_buf.lock().expect("events lock").clone();
    // The loop may finish the current batch before observing the cancel; what
    // matters is that it stops instead of draining all three scripted turns.
    assert!(
        total.load(Ordering::SeqCst) < 3,
        "a hard abort stops the run instead of running every turn: {events:?}"
    );
    assert!(run.is_ok(), "abort resolves Ok, not an error: {run:?}");
}
/// A press with nothing installed reports `NothingToInterrupt` *and* arms the
/// latch, so the note the CLI shows must say the next step is affected rather
/// than "nothing to interrupt".
///
/// This is the agent-side half of that contract: the switch the note describes
/// really does cancel the next action.
#[test]
fn a_press_with_nothing_installed_interrupts_the_next_action() {
    let interrupt = InterruptControl::new();
    assert!(!interrupt.has_current(), "nothing is in flight");

    // What `AgentHarness::interrupt()` does in this arm.
    interrupt.soft_interrupt();
    assert!(!interrupt.has_current(), "still nothing in flight");

    let next_action = CancellationToken::new();
    let id = interrupt.install(next_action.clone());
    assert!(
        next_action.is_cancelled(),
        "the latch cancels the next action — `NothingToInterrupt` promises this"
    );

    // ...and the loop drains it at that action's boundary, so it hits once.
    assert!(interrupt.take_soft(), "the loop can tell why it died");
    interrupt.clear(id);
    let following = CancellationToken::new();
    let _id = interrupt.install(following.clone());
    assert!(
        !following.is_cancelled(),
        "a one-shot latch must not cancel every later action"
    );
}

// ---------------------------------------------------------------------------
// Phase 2: the model stream is soft-interruptible too
// ---------------------------------------------------------------------------

/// What a real provider does under cancellation, plus the bookkeeping the loop's
/// behaviour depends on but no event exposes.
///
/// Request 0 is the interruptible one: it streams a partial and then waits for
/// its token, exactly like an HTTP request waiting to be cancelled. Later
/// requests answer normally so a test can prove the loop kept going.
#[derive(Clone, Default)]
struct FauxProvider {
    /// Requests made, incremented synchronously inside the `StreamFn` closure.
    requests: Arc<AtomicUsize>,
    /// Requests that died on cancellation. Incremented *before* the terminal
    /// event is pushed, so a hook the loop runs afterwards (steering drain,
    /// classification) cannot read `0` by racing the wakeup.
    interrupted: Arc<AtomicUsize>,
    /// Role tags of the `ctx.messages` every request was given, in request order.
    /// This is the only honest way to assert "the aborted attempt never entered
    /// the LLM context": the loop's `AgentContext` is moved into the run.
    seen: Arc<std::sync::Mutex<Vec<Vec<&'static str>>>>,
}

impl FauxProvider {
    fn stream_fn(&self) -> kiqi_agent::StreamFn {
        let provider = self.clone();
        kiqi_agent::stream_fn(move |_model, ctx, opts| {
            let provider = provider.clone();
            let signal = opts.signal.clone();
            let index = provider.requests.fetch_add(1, Ordering::SeqCst);
            provider
                .seen
                .lock()
                .expect("seen lock")
                .push(ctx.messages.iter().map(role_tag).collect());

            let (mut prod, stream) = kiqi_ai::event_stream::create_assistant_message_event_stream();
            tokio::spawn(async move {
                if index > 0 {
                    let done = common::assistant_text("after-interrupt", StopReason::Stop);
                    prod.push(AssistantMessageEvent::Start {
                        partial: Arc::new(done.clone()),
                    });
                    prod.push(AssistantMessageEvent::Done {
                        reason: DoneReason::Stop,
                        message: done,
                    });
                    return;
                }

                // First request: a partial with a few deltas, i.e. a stream the
                // UI has visibly started rendering.
                let partial = Arc::new(common::assistant_text(
                    "half a thought",
                    StopReason::Pending,
                ));
                prod.push(AssistantMessageEvent::Start {
                    partial: Arc::clone(&partial),
                });
                prod.push(AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: Arc::clone(&partial),
                });
                for chunk in ["half ", "a ", "thought"] {
                    prod.push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: chunk.to_string(),
                        partial: Arc::clone(&partial),
                    });
                }

                // Wait to be cancelled. The timeout is a safety net so a broken
                // interrupt fails the assertion instead of hanging the suite —
                // and it reports a *failure*, not a cancellation, so a loop that
                // hangs the provider on the wrong token cannot look "soft".
                match tokio::time::timeout(Duration::from_secs(5), signal.cancelled()).await {
                    Ok(()) => {
                        provider.interrupted.fetch_add(1, Ordering::SeqCst);
                        prod.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Aborted,
                            error: common::assistant_text("half a thought", StopReason::Aborted),
                        });
                    }
                    Err(_) => {
                        prod.push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Error,
                            error: common::assistant_text("half a thought", StopReason::Error),
                        });
                    }
                }
            });
            stream
        })
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn seen(&self) -> Vec<Vec<&'static str>> {
        self.seen.lock().expect("seen lock").clone()
    }
}

fn role_tag(message: &Message) -> &'static str {
    match message {
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "toolResult",
    }
}

/// What `AgentHarness::interrupt()`'s soft arm does, fired as soon as a stream
/// request is the installed action.
fn soft_interrupter(interrupt: &Arc<InterruptControl>) -> tokio::task::JoinHandle<bool> {
    let interrupt = Arc::clone(interrupt);
    tokio::spawn(async move {
        for _ in 0..5_000 {
            if interrupt.has_current() {
                interrupt.soft_interrupt();
                return true;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        false
    })
}

/// A hard abort: cancels the *run* signal, not the request token.
fn run_aborter(
    interrupt: &Arc<InterruptControl>,
    handle: &kiqi_agent::AbortHandle,
) -> tokio::task::JoinHandle<bool> {
    let interrupt = Arc::clone(interrupt);
    let handle = handle.clone();
    tokio::spawn(async move {
        for _ in 0..5_000 {
            if interrupt.has_current() {
                handle.abort();
                return true;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        false
    })
}

fn text_of(message: &AgentMessage) -> String {
    match message {
        AgentMessage::Assistant(a) => a
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect(),
        AgentMessage::User(u) => match &u.content {
            kiqi_ai::types::UserContent::Text(t) => t.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// The load-bearing invariant of phase 2: a soft-interrupted stream leaves
/// nothing behind. Deleting the `take_soft()` (so the branch never fires) makes
/// this fail on `new_messages` carrying the aborted assistant; deleting the
/// `context.messages.pop()` makes the follow-up request in
/// `a_stream_soft_interrupt_continues_with_queued_steering` see it.
#[tokio::test]
async fn a_stream_soft_interrupt_drops_the_partial_message() {
    let interrupt = Arc::new(InterruptControl::new());
    let provider = FauxProvider::default();

    let mut config = common::base_config();
    config.interrupt = Arc::clone(&interrupt);
    let handle = kiqi_agent::AbortHandle::new();
    config.signal = handle.token();

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let prompt = common::user_message("hello");
    let (collector, events_buf) = kiqi_agent::CollectorEmitter::new();
    let emit: Arc<dyn kiqi_agent::AgentEmitter> = Arc::new(collector);

    let interrupter = soft_interrupter(&interrupt);
    let run =
        kiqi_agent::run_agent_loop(vec![prompt], context, config, emit, provider.stream_fn()).await;

    assert!(
        interrupter.await.expect("interrupter task"),
        "the stream request was never installed"
    );
    let new_messages = run.expect("a soft interrupt resolves Ok, not an error");
    let events = events_buf.lock().expect("events lock").clone();

    // 1. Nothing finished: the prompt, and no assistant at all.
    assert_eq!(
        new_messages.len(),
        1,
        "only the prompt survives: {new_messages:?}"
    );
    assert!(
        matches!(new_messages[0], AgentMessage::User(_)),
        "no assistant message is persisted: {new_messages:?}"
    );

    // 2. AgentEnd carries the same vec the harness persists, so the session file
    //    (and the TUI transcript) never learn about the attempt.
    let agent_end = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::AgentEnd { messages } => Some(messages.clone()),
            _ => None,
        })
        .expect("AgentEnd was emitted");
    assert!(
        !agent_end
            .iter()
            .any(|m| matches!(m, AgentMessage::Assistant(_))),
        "AgentEnd (= what the harness persists) holds no assistant: {agent_end:?}"
    );

    // 3. The UI still gets its `MessageEnd`, so the live streaming block is
    //    finalized instead of looking stuck. That is the only place the aborted
    //    message is allowed to appear.
    let ended = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(a),
            } => Some(a.stop_reason),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ended,
        vec![StopReason::Aborted],
        "the TUI's finalize hook still fires for the interrupted block: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::MessageUpdate { .. })),
        "deltas were streamed before the interrupt: {events:?}"
    );

    // 4. The run survived: nothing cancelled it, and the loop stopped asking
    //    instead of spinning on repeated aborted requests.
    assert!(
        !handle.token().is_cancelled(),
        "a soft interrupt must not cancel the run signal"
    );
    assert_eq!(
        provider.requests(),
        1,
        "the loop stopped the turn instead of re-requesting"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnEnd { .. })),
        "the interrupted attempt is not a turn: {events:?}"
    );
    assert!(
        !interrupt.has_current(),
        "the request slot was released (clear), so a later press targets the next action"
    );
}

/// The other half of "soft": a message queued while the stream was dying is not
/// stranded. The loop injects it and asks the provider again.
#[tokio::test]
async fn a_stream_soft_interrupt_continues_with_queued_steering() {
    let interrupt = Arc::new(InterruptControl::new());
    let provider = FauxProvider::default();

    let mut config = common::base_config();
    config.interrupt = Arc::clone(&interrupt);
    let handle = kiqi_agent::AbortHandle::new();
    config.signal = handle.token();
    // Deliver the steering message only after the interrupt landed — the initial
    // drain at the top of `run_loop` must see an empty queue.
    let delivered = Arc::new(AtomicUsize::new(0));
    let steering: kiqi_agent::GetSteeringMessages = {
        let provider = provider.clone();
        let delivered = Arc::clone(&delivered);
        Arc::new(move || {
            let provider = provider.clone();
            let delivered = Arc::clone(&delivered);
            Box::pin(async move {
                if provider.interrupted.load(Ordering::SeqCst) > 0
                    && delivered.fetch_add(1, Ordering::SeqCst) == 0
                {
                    vec![common::user_message("keep going")]
                } else {
                    Vec::new()
                }
            })
        })
    };
    config.get_steering_messages = Some(steering);

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let prompt = common::user_message("hello");
    let (collector, events_buf) = kiqi_agent::CollectorEmitter::new();
    let emit: Arc<dyn kiqi_agent::AgentEmitter> = Arc::new(collector);

    let interrupter = soft_interrupter(&interrupt);
    let run =
        kiqi_agent::run_agent_loop(vec![prompt], context, config, emit, provider.stream_fn()).await;

    assert!(
        interrupter.await.expect("interrupter task"),
        "the stream request was never installed"
    );
    let new_messages = run.expect("the run survives the soft interrupt");
    let events = events_buf.lock().expect("events lock").clone();

    // The steering message was injected and answered: prompt, steering, answer.
    assert_eq!(
        new_messages.len(),
        3,
        "prompt + injected steering + the next answer: {new_messages:?}"
    );
    assert!(matches!(new_messages[0], AgentMessage::User(_)));
    assert_eq!(text_of(&new_messages[1]), "keep going");
    assert!(matches!(new_messages[1], AgentMessage::User(_)));
    assert!(
        matches!(new_messages[2], AgentMessage::Assistant(_)),
        "the provider was asked again: {new_messages:?}"
    );
    assert_eq!(text_of(&new_messages[2]), "after-interrupt");

    // The provider really saw two requests, and the second one's context proves
    // the interrupted attempt was dropped (`context.messages.pop()`): prompt +
    // steering, no assistant.
    assert_eq!(provider.requests(), 2, "one request per attempt");
    assert_eq!(
        provider.seen(),
        vec![vec!["user"], vec!["user", "user"]],
        "the aborted attempt must not leak into the next request"
    );

    assert!(
        !handle.token().is_cancelled(),
        "a soft interrupt must not cancel the run signal"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentEnd { .. })));
    assert!(
        !interrupt.has_current(),
        "the second request released its slot too"
    );
}

/// The distinction that makes the above safe: cancelling the *run* signal is a
/// hard abort and must keep today's semantics — the `Aborted` assistant message
/// is a real message, it is persisted, and the run ends.
#[tokio::test]
async fn a_hard_stream_abort_still_ends_the_run() {
    let interrupt = Arc::new(InterruptControl::new());
    let provider = FauxProvider::default();

    let mut config = common::base_config();
    config.interrupt = Arc::clone(&interrupt);
    let handle = kiqi_agent::AbortHandle::new();
    config.signal = handle.token();

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let prompt = common::user_message("hello");
    let (collector, events_buf) = kiqi_agent::CollectorEmitter::new();
    let emit: Arc<dyn kiqi_agent::AgentEmitter> = Arc::new(collector);

    let aborter = run_aborter(&interrupt, &handle);
    let run =
        kiqi_agent::run_agent_loop(vec![prompt], context, config, emit, provider.stream_fn()).await;

    assert!(
        aborter.await.expect("aborter task"),
        "the stream request was never installed"
    );
    let new_messages = run.expect("abort resolves Ok, not an error");
    let events = events_buf.lock().expect("events lock").clone();

    // The aborted assistant is kept: a hard abort records what happened.
    let last = new_messages.last().expect("the prompt at least");
    match last {
        AgentMessage::Assistant(a) => assert_eq!(
            a.stop_reason,
            StopReason::Aborted,
            "the aborted assistant is persisted, not dropped: {new_messages:?}"
        ),
        other => panic!("expected the aborted assistant last, got {other:?}"),
    }

    assert!(
        handle.token().is_cancelled(),
        "the run signal was cancelled"
    );
    // The abort path still closes the turn (the soft path deliberately does not).
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnEnd { .. })),
        "abort ends the turn: {events:?}"
    );
    assert_eq!(
        provider.requests(),
        1,
        "an aborted run does not ask the provider again"
    );
}

/// The latch (`take_soft`) is load-bearing: "soft" is what
/// `InterruptControl::soft_interrupt()` *declares*, not what the loop can infer
/// from "a child token died but the run signal did not".
///
/// A request that dies some other way — a provider-side per-request watchdog
/// cancelling the token it was handed is the shape this fakes — must keep the
/// pre-phase-2 semantics: the `Aborted` message is a real message, it is
/// persisted, and the run ends. Treating it as a soft interrupt would silently
/// swallow a failed turn.
#[tokio::test]
async fn a_request_killed_without_the_soft_flag_is_not_swallowed() {
    let requests = Arc::new(AtomicUsize::new(0));
    let stream_fn = {
        let requests = Arc::clone(&requests);
        kiqi_agent::stream_fn(move |_model, _ctx, opts| {
            let requests = Arc::clone(&requests);
            let signal = opts.signal.clone();
            let (mut prod, stream) = kiqi_ai::event_stream::create_assistant_message_event_stream();
            tokio::spawn(async move {
                requests.fetch_add(1, Ordering::SeqCst);
                prod.push(AssistantMessageEvent::Start {
                    partial: Arc::new(common::assistant_text("", StopReason::Pending)),
                });
                // Nobody called `soft_interrupt()`: this is the request's own
                // cancellation, and it must not be mistaken for a user press.
                signal.cancel();
                prod.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Aborted,
                    error: common::assistant_text("gave up", StopReason::Aborted),
                });
            });
            stream
        })
    };

    let mut config = common::base_config();
    let handle = kiqi_agent::AbortHandle::new();
    config.signal = handle.token();

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
    };
    let (collector, events_buf) = kiqi_agent::CollectorEmitter::new();
    let emit: Arc<dyn kiqi_agent::AgentEmitter> = Arc::new(collector);

    let run = kiqi_agent::run_agent_loop(
        vec![common::user_message("hello")],
        context,
        config,
        emit,
        stream_fn,
    )
    .await
    .expect("the run resolves Ok");
    let events = events_buf.lock().expect("events lock").clone();

    match run.last() {
        Some(AgentMessage::Assistant(a)) => assert_eq!(
            a.stop_reason,
            StopReason::Aborted,
            "an unmarked cancellation keeps the aborted assistant: {run:?}"
        ),
        other => panic!("expected the aborted assistant last, got {other:?}"),
    }
    assert_eq!(text_of(run.last().unwrap()), "gave up");
    assert!(
        !handle.token().is_cancelled(),
        "the request token, not the run signal, was cancelled"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TurnEnd { .. })),
        "the failed attempt closed its turn: {events:?}"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1, "no retry loop on abort");
}
