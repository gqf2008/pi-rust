//! Soft-interrupt audit trace: the harness side (`decision=…`).
//!
//! A separate integration-test binary, like the agent-side trace test, because
//! `KI_DEBUG_INTERRUPT` is resolved once per process into a `OnceLock`: sharing a
//! test binary with other `interrupt()` callers would let one of them freeze the
//! destination before this test sets it.
//!
//! The scenario is the real one users hit — a tool call in flight, Esc pressed —
//! so the verdict line is produced by the shipped path, not a stub: `press` from
//! `InterruptControl`, `decision` from `AgentHarness::interrupt`, and a second
//! press after the run resolves to pin the `no-run` verdict.

use std::sync::{Arc, Mutex};

use kiqi_ai::providers::faux::{FauxProvider, FauxScript};
use kiqi_ai::Provider;
use kiqi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use kiqi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use kiqi_harness::session::types::SessionMetadata;
use kiqi_harness::session::{DefaultIdGenerator, Session};
use kiqi_harness::types::{AgentHarnessOptions, HarnessTool, RetryPolicy};

/// A tool that blocks until the loop's per-action token is cancelled.
struct InterruptibleTool {
    schema: kiqi_ai::types::Tool,
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl InterruptibleTool {
    fn new() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            Self {
                schema: kiqi_ai::types::Tool {
                    name: "slow_tool".to_string(),
                    description: "blocks until its action token is cancelled".to_string(),
                    parameters: kiqi_ai::types::Schema::new(
                        serde_json::json!({ "type": "object", "properties": {} }),
                    ),
                    constrained_sampling: None,
                },
                started: Mutex::new(Some(tx)),
            },
            rx,
        )
    }
}

#[async_trait::async_trait]
impl kiqi_agent::AgentTool for InterruptibleTool {
    fn schema(&self) -> &kiqi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "slow_tool"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        signal: tokio_util::sync::CancellationToken,
        _on_update: Arc<dyn Fn(kiqi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<kiqi_agent::AgentToolResult, kiqi_agent::AgentError> {
        if let Some(tx) = self.started.lock().unwrap().take() {
            let _ = tx.send(());
        }
        signal.cancelled().await;
        Ok(kiqi_agent::AgentToolResult::text("tool stopped early"))
    }
}

async fn harness_with_interruptible_tool() -> (Arc<AgentHarness>, tokio::sync::oneshot::Receiver<()>)
{
    let script = FauxScript::new()
        .with_tool_call("slow_tool", serde_json::json!({}))
        .with_text("Finished after the tool was interrupted.");
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();
    let (tool, started) = InterruptibleTool::new();
    let tools: Vec<HarnessTool> = vec![HarnessTool::new(Arc::new(tool))];
    let active: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();

    let metadata = SessionMetadata {
        id: "interrupt-trace".into(),
        created_at: 0,
        parent_session_id: None,
    };
    let storage = Arc::new(InMemorySessionStorage::new(
        metadata,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    let session = Session::new(storage, None);
    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: active,
        tools,
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: RetryPolicy::default(),
        compaction: Default::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        models: vec![provider.clone() as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
        allow_existing_session: false,
    };
    let harness = Arc::new(AgentHarness::create(options).await.expect("create harness"));
    (harness, started)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_records_the_verdict_of_each_press() {
    let path = std::env::temp_dir().join(format!(
        "kiqi-interrupt-trace-harness-{}.log",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    std::env::set_var("KI_DEBUG_INTERRUPT", &path);

    let (harness, started) = harness_with_interruptible_tool().await;

    let run_harness = Arc::clone(&harness);
    let run = tokio::spawn(async move {
        AgentLane::prompt_text(run_harness.as_ref(), "run the slow tool", vec![]).await
    });
    started.await.expect("the tool must have started");

    let interrupted = AgentLane::interrupt(harness.as_ref())
        .await
        .expect("a soft interrupt is best-effort");
    assert!(
        matches!(
            interrupted,
            kiqi_harness::agent_harness::InterruptResult::Soft { .. }
        ),
        "the press reached the tool: {interrupted:?}"
    );

    let result = run
        .await
        .expect("the run task must not panic")
        .expect("the run must resolve");
    assert!(
        matches!(result.outcome, HarnessRunOutcome::Completed { .. }),
        "a soft interrupt must let the run finish; got {:?}",
        result.outcome
    );

    // A press after the run resolved: the trace must show the idle verdict.
    assert_eq!(
        AgentLane::interrupt(harness.as_ref())
            .await
            .expect("best effort"),
        kiqi_harness::agent_harness::InterruptResult::NoActiveRun
    );

    let log = std::fs::read_to_string(&path).expect("the trace file was written");
    let _ = std::fs::remove_file(&path);
    assert!(
        log.contains("press installed=true kind=Tool"),
        "the trace must show what the press found installed: {log}"
    );
    assert!(
        log.contains("decision=soft kind=Tool"),
        "the harness verdict must name the tool it stopped: {log}"
    );
    assert!(
        log.contains("decision=no-run kind=none"),
        "a press with no active run must be recorded as such: {log}"
    );
}
