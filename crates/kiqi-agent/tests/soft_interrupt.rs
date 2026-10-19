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

#[path = "common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::assistant_tool_calls;
use kiqi_agent::interrupt::InterruptControl;
use kiqi_agent::{AgentContext, AgentEvent, AgentTool, AgentToolResult};
use kiqi_ai::types::{StopReason, Tool};
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
