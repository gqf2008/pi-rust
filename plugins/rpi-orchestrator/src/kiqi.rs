//! The compile-time (`kiqi-plugin`) face of rpi-orchestrator.
//!
//! Thin by design: the three tool builders already exist and, behind the
//! cdylib ABI, each already runs on a worker thread. This face reproduces the
//! same shape for [`Plugin::call`] — the builder runs on a worker, the call
//! pumps the worker's progress channel into the host's sink, and the host's
//! cancellation probe drives the plugin's [`Control`]. A cancel also flips the
//! run-wide abort flag, so a dispatch wave's background nodes die with the
//! call — exactly the ABI `cancel()` semantics; a *normal* completion does
//! not, so an in-flight wave survives between calls.

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use kiqi_plugin::{
    Block, HostCaps, Plugin, PluginError, Progress, ProgressSink, ToolOutcome, ToolSpec,
};
use serde_json::Value;

use crate::control::Control;
use crate::kit::{Builder, Update};

/// How often the pump loop re-checks cancellation. Matches the spawn wait
/// loop's slice, so a cancel is noticed within the same budget.
const POLL_SLICE: Duration = Duration::from_millis(100);

pub struct OrchestratorPlugin;

impl OrchestratorPlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for OrchestratorPlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(OrchestratorPlugin::new())
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    parameters: &'static str,
    builder: Builder,
}

fn tools() -> &'static [ToolDef] {
    static TOOLS: &[ToolDef] = &[
        ToolDef {
            name: crate::SUBAGENT_TOOL,
            description: crate::SUBAGENT_DESCRIPTION,
            parameters: crate::SUBAGENT_PARAMS,
            builder: crate::subagent::subagent,
        },
        ToolDef {
            name: "plan",
            description: crate::PLAN_DESCRIPTION,
            parameters: crate::PLAN_PARAMS,
            builder: crate::plan::plan,
        },
        ToolDef {
            name: "dispatch",
            description: crate::DISPATCH_DESCRIPTION,
            parameters: crate::DISPATCH_PARAMS,
            builder: crate::dispatch::dispatch,
        },
    ];
    TOOLS
}

impl Plugin for OrchestratorPlugin {
    fn id(&self) -> &str {
        "rpi-orchestrator"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        tools()
            .iter()
            .map(|def| ToolSpec {
                name: def.name.to_string(),
                description: def.description.to_string(),
                parameters: serde_json::from_str(def.parameters)
                    .expect("the tool schemas are valid JSON"),
            })
            .collect()
    }

    fn call(
        &self,
        name: &str,
        arguments: &Value,
        _caps: &HostCaps<'_>,
        sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        let Some(def) = tools().iter().find(|def| def.name == name) else {
            return Err(PluginError::UnknownTool(name.to_string()));
        };
        let result = run_builder(def.builder, arguments, _caps, sink)?;
        outcome_from(result)
    }

    fn before_agent_start(&self, prompt: &Value, _caps: &HostCaps<'_>) -> Option<String> {
        crate::guide_for_prompt(prompt)
    }
}

/// Run one builder on a worker thread, pumping its progress to the sink and
/// watching the host's cancellation probe.
fn run_builder(
    builder: Builder,
    arguments: &Value,
    caps: &HostCaps<'_>,
    sink: &dyn ProgressSink,
) -> Result<Result<String, String>, PluginError> {
    let (tx, rx) = mpsc::channel::<Update>();
    let control = Control::default();
    let worker = {
        let control = control.clone();
        let arguments = arguments.clone();
        std::thread::spawn(move || {
            let result = builder(&arguments, &control, &tx);
            let _ = tx.send(Update::Done(result));
        })
    };
    let result = loop {
        match rx.recv_timeout(POLL_SLICE) {
            Ok(Update::Progress(wrapped)) => sink.progress(Progress::Text(unwrap_text(wrapped))),
            Ok(Update::Done(result)) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err("the worker finished without a result".to_string())
            }
        }
        if let Some(probe) = caps.cancel {
            if probe.is_cancelled() {
                control.cancel();
                // The run-wide abort: a dispatch wave's background nodes link
                // to it, so a host abort kills in-flight children with the
                // call (the ABI cancel() did the same).
                crate::host_abort_flag().store(true, Ordering::SeqCst);
            }
        }
    };
    if worker.join().is_err() {
        return Err(PluginError::Failed("the tool worker panicked".to_string()));
    }
    Ok(result)
}

/// A builder result is the cdylib wire shape `{"content":[{"type":"text",
/// "text":…}]}` (optionally with `details`); turn it into a `ToolOutcome`.
fn outcome_from(result: Result<String, String>) -> Result<ToolOutcome, PluginError> {
    let json = result.map_err(PluginError::Failed)?;
    let value: Value = serde_json::from_str(&json)
        .map_err(|_| PluginError::Failed(format!("bad tool result: {json}")))?;
    let text = unwrap_text(json.clone());
    Ok(ToolOutcome {
        blocks: vec![Block::Text(text)],
        is_error: false,
        details: value.get("details").cloned().unwrap_or(Value::Null),
    })
}

/// Extract the text of a `{"content":[{"type":"text","text":…}]}` envelope;
/// a string that is not the envelope comes back as-is.
fn unwrap_text(wrapped: String) -> String {
    serde_json::from_str::<Value>(&wrapped)
        .ok()
        .and_then(|value| {
            value
                .get("content")
                .and_then(Value::as_array)
                .and_then(|blocks| blocks.first())
                .and_then(|block| block.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or(wrapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_plugin::{CancelProbe, Log, LogLevel, NoProgress};

    struct TestLog;

    impl Log for TestLog {
        fn log(&self, _level: LogLevel, _message: &str) {}
    }

    /// `HOST_ABORT` is process-wide; the two tests that touch it must not race
    /// (one asserts it flipped, the other resets it per run start).
    static ABORT_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A builder that parks until cancelled — drives the probe→Control wiring.
    fn parking_builder(
        _params: &Value,
        control: &Control,
        progress: &crate::kit::Progress,
    ) -> Result<String, String> {
        for _ in 0..200 {
            control.check()?;
            crate::kit::progress(progress, "tick");
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(crate::kit::content_only("never reached"))
    }

    struct AlreadyCancelled;

    impl CancelProbe for AlreadyCancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn declares_the_three_tools_with_valid_schemas() {
        let plugin = OrchestratorPlugin::new();
        let tools = plugin.tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["subagent", "plan", "dispatch"]);
    }

    #[test]
    fn an_unknown_tool_is_an_error_and_validation_happens_before_spawning() {
        let plugin = OrchestratorPlugin::new();
        let log = TestLog;
        let caps = HostCaps::observer(&log);
        assert!(matches!(
            plugin.call("nope", &Value::Null, &caps, &NoProgress),
            Err(PluginError::UnknownTool(_))
        ));
        // An empty task is refused by the builder itself — no child spawned.
        let error = plugin
            .call(
                "subagent",
                &serde_json::json!({ "task": " " }),
                &caps,
                &NoProgress,
            )
            .unwrap_err();
        assert!(error.to_string().contains("empty"), "{error}");
    }

    /// The harness's cancel probe drives the plugin's Control: a cancelled
    /// call unwinds the builder instead of running it to completion.
    #[test]
    fn a_cancelled_probe_unwinds_the_builder() {
        let _guard = ABORT_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let log = TestLog;
        let probe = AlreadyCancelled;
        let caps = HostCaps {
            cancel: Some(&probe),
            ..HostCaps::observer(&log)
        };
        crate::host_abort_flag().store(false, Ordering::SeqCst);
        let result = run_builder(parking_builder, &Value::Null, &caps, &NoProgress)
            .expect("the worker itself is fine")
            .expect_err("the builder must have unwound");
        assert_eq!(result, "cancelled");
        // And the run-wide abort flipped with it (background nodes die too).
        assert!(crate::host_abort_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn a_plain_result_becomes_text_and_details_ride_along() {
        let with_details =
            crate::kit::result_with_details("graph shown", serde_json::json!({"nodes": 2}));
        let outcome = outcome_from(Ok(with_details)).expect("outcome");
        assert!(matches!(outcome.blocks[0], Block::Text(ref t) if t == "graph shown"));
        assert_eq!(outcome.details["nodes"], 2);

        let plain = outcome_from(Ok(crate::kit::content_only("hello"))).expect("outcome");
        assert!(matches!(plain.blocks[0], Block::Text(ref t) if t == "hello"));
        assert!(plain.details.is_null());
    }

    #[test]
    fn before_agent_start_leaves_the_prompt_alone_without_a_goal() {
        let _guard = ABORT_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        // The test process has no KI_ORCHESTRATOR_GUIDE and no .ki/goals.json
        // in its working directory, so the guidance stays off.
        let plugin = OrchestratorPlugin::new();
        let log = TestLog;
        let caps = HostCaps::observer(&log);
        assert_eq!(
            plugin.before_agent_start(&serde_json::json!({"systemPrompt": "base"}), &caps),
            None
        );
    }
}
