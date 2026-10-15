#![forbid(unsafe_code)]
// The cdylib entrypoint (the SDK's `export_plugin!`) necessarily dereferences
// the host's registration-callback pointers; the raw-pointer handling lives in
// the SDK, not here. Same allowance the other cdylib packages carry.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

//! `rpi-orchestrator` — DAG subagent orchestration for the rpi Rust agent.
//!
//! A subagent is a child `rpi` process. Three tools share one session-scoped
//! task graph:
//!
//! - `subagent` — run one bounded task (the delegation primitive),
//! - `plan`     — author/edit the DAG,
//! - `dispatch` — run the ready set (one wave per call).
//!
//! The graph lives in this process's memory for the life of a run. Switching
//! sessions clears it — a human decides whether to resume or start over, rather
//! than the extension silently replaying an old branch. `plan`/`dispatch`
//! results still carry the graph in `details` for inspection. Design and
//! stages: `DESIGN.md`.

pub mod kiqi;

mod control;
mod dispatch;
mod graph;
mod kit;
mod plan;
mod spawn;
mod subagent;

use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::control::Control;

static GRAPH: OnceLock<Mutex<graph::Graph>> = OnceLock::new();
static RUNNING: OnceLock<Mutex<HashMap<String, Control>>> = OnceLock::new();

/// Set when the host cancels a tool call (a run abort). Node workers link to this
/// so an abort kills in-flight children, while a *normal* tool-call completion
/// (`destroy`, which also cancels the call's own token) does not.
static HOST_ABORT: OnceLock<Arc<AtomicBool>> = OnceLock::new();

pub(crate) fn host_abort_flag() -> Arc<AtomicBool> {
    Arc::clone(HOST_ABORT.get_or_init(|| Arc::new(AtomicBool::new(false))))
}

/// Bumped whenever the graph is replaced (`plan set`), so a worker from the old
/// graph cannot write its result onto a new node that reuses the same id.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn current_generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

pub(crate) fn bump_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// A node token linked to the host-abort signal (and independently cancellable).
pub(crate) fn node_control() -> Control {
    Control::child_of(&Control::from_flag(host_abort_flag()))
}

/// Node id → its cancellation token, for every node currently running. Lets a
/// mid-wave `plan cancel` stop one node without stopping the whole call.
pub(crate) fn running() -> &'static Mutex<HashMap<String, Control>> {
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The one in-memory graph. A poisoned lock is recovered rather than panicking,
/// mirroring the SDK's tool-handle convention.
pub(crate) fn graph_state() -> &'static Mutex<graph::Graph> {
    GRAPH.get_or_init(|| Mutex::new(graph::Graph::default()))
}

crate::tool_entry!(subagent_tool, crate::subagent::subagent);
crate::tool_entry!(plan_tool, crate::plan::plan);
crate::tool_entry!(dispatch_tool, crate::dispatch::dispatch);

pub(crate) const SUBAGENT_TOOL: &str = "subagent";
pub(crate) const SUBAGENT_DESCRIPTION: &str =
    "Delegate one bounded task to a fresh ki subprocess and return its final answer.";
pub(crate) const SUBAGENT_PARAMS: &str = r#"{"type":"object","properties":{"task":{"type":"string","maxLength":20000},"role":{"type":"string"},"provider":{"type":"string"},"model":{"type":"string"},"maxTurns":{"type":"integer","minimum":1,"maximum":32},"timeoutSeconds":{"type":"integer","minimum":10,"maximum":1800}},"required":["task"]}"#;

pub(crate) const PLAN_DESCRIPTION: &str = "Author or edit the task DAG. action=set (replace with `nodes`), action=update (addNodes/removeIds/cancel/setDeps/reset), action=show. A node may declare `expectFile` (file that must exist after its child runs) and `verify` (shell command that must exit 0); `dispatch` enforces both. Completion is written only by `dispatch`, never by `plan`.";
pub(crate) const PLAN_PARAMS: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["set","update","show"]},"nodes":{"type":"array","description":"action=set: full node list. Each node: {id, task, deps?, role?, provider?, model?, maxTurns?, timeoutSeconds?, expectFile?, verify?}"},"addNodes":{"type":"array","description":"action=update: nodes to add (same node shape)"},"removeIds":{"type":"array","items":{"type":"string"},"description":"action=update: ids to remove (a running node is cancelled first)"},"cancel":{"type":"array","items":{"type":"string"},"description":"action=update: stop these nodes — a running child is killed and the node fails, a pending node is skipped"},"setDeps":{"type":"array","description":"action=update: [{id, deps}]"},"reset":{"type":"array","items":{"type":"string"},"description":"action=update: re-arm these ids to pending for another attempt (not allowed while a node is running)"}},"required":["action"]}"#;

pub(crate) const DISPATCH_DESCRIPTION: &str = "Schedule the plan: start every ready node within `maxParallel`, then (per `waitSeconds`) wait and return the results of nodes that finished. Nodes run in the background, so with `waitSeconds=0` you can reshape the graph (`plan` cancel/addNodes) while a wave is in flight and call dispatch again to collect.";
pub(crate) const DISPATCH_PARAMS: &str = r#"{"type":"object","properties":{"maxParallel":{"type":"integer","minimum":1,"maximum":16,"description":"max concurrent subagents (default 4), counted across everything currently running"},"waitSeconds":{"type":"integer","minimum":0,"description":"0 = start ready nodes and return at once (reshape the graph meanwhile); N = wait up to N seconds for running nodes; omit to wait until the running set drains"}}}"#;

/// Optional override for the orchestration guidance injected into the system
/// prompt. Guidance is **on by default whenever a goal is active** — `/goal`
/// implies orchestration, so no variable is needed. This only forces the decision:
/// `0/false/no/off` disables it even with a goal, `1/true/yes/on` enables it even
/// without one.
pub const GUIDE_ENV: &str = "KI_ORCHESTRATOR_GUIDE";

/// Project-relative path of `rpi-goal`'s state. An active goal there is what turns
/// the guidance on — a deliberate coupling to `/goal` mode.
const GOALS_FILE: &str = ".ki/goals.json";

const GUIDE_HEADING: &str = "## Subagent orchestration";

const GUIDE_SECTION: &str = r#"## Subagent orchestration

For work that has several steps, or steps that can run in parallel, use the
`plan` and `dispatch` tools instead of doing everything inline:

1. `plan` with action=set: write the task graph as nodes, each with an `id`, a
   self-contained `task`, and its `deps`.
2. `dispatch`: run the current ready wave. Nodes whose dependencies are done run
   in parallel (up to `maxParallel`).
3. Read the results, then `plan` with action=update (add/remove/re-dep/reset
   nodes) and `dispatch` again until the goal is met.

A subagent starts fresh and cannot see this conversation, so each node's `task`
must contain everything it needs. Prefer this over hand-rolling the whole
workflow with `bash`."#;

/// Tri-state read of [`GUIDE_ENV`]: `Some(true)` force on, `Some(false)` force
/// off, `None` means "decide from the goal state".
fn guide_override(value: Option<&str>) -> Option<bool> {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("1" | "true" | "yes" | "on") => Some(true),
        Some("0" | "false" | "no" | "off") => Some(false),
        _ => None,
    }
}

/// Whether `rpi-goal` has an active goal in this project. Parsed leniently: any
/// read or parse problem simply means "no goal".
fn active_goal_from(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(active) = value.get("active") else {
        return false;
    };
    !active.is_null() && active.get("status").and_then(Value::as_str) == Some("active")
}

fn active_goal_in_project() -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    match std::fs::read_to_string(cwd.join(GOALS_FILE)) {
        Ok(text) => active_goal_from(&text),
        Err(_) => false,
    }
}

fn guide_enabled() -> bool {
    // Never inject into a subagent: it has no `plan`/`dispatch`, and telling it to
    // use them would just waste a turn.
    if std::env::var(crate::spawn::SUBAGENT_ENV).is_ok() {
        return false;
    }
    // `/goal` implies orchestration: an active goal turns the guidance on unless
    // the variable explicitly overrides the decision.
    guide_override(std::env::var(GUIDE_ENV).ok().as_deref()).unwrap_or_else(active_goal_in_project)
}

/// Only the cdylib handler writes through the ABI output; under the
/// `compile-time` feature (which cfg's the entrypoint out) this is dead.
#[allow(dead_code)]
fn write_out(out: &mut rpi_plugin_sdk::callbacks::Output<'_>, value: Value) {
    if out.is_available() {
        out.write(value.to_string());
    }
}

/// The session the graph belongs to. The graph is process-wide statics (a
/// compile-time plugin is a process singleton), and the documented contract is
/// "the graph lives for the life of a run and is cleared when the session is
/// switched" — so the run start tracks the event's `sessionId` and clears the
/// state when it changes. Nothing else resets the graph.
static LAST_SESSION_ID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Cancel every running node, drop the graph, and invalidate in-flight
/// workers. Used when the session switches; the old run's children are killed
/// rather than left to commit into a conversation that can no longer see them.
fn clear_run_state() {
    let controls: Vec<Control> = running()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain()
        .map(|(_, control)| control)
        .collect();
    for control in controls {
        control.cancel();
    }
    let mut graph = graph_state().lock().unwrap_or_else(|p| p.into_inner());
    *graph = graph::Graph::default();
    drop(graph);
    bump_generation();
}

/// Decide the system prompt for a run start, shared by the cdylib and
/// compile-time faces: `Some(next)` when the guidance section was appended,
/// `None` to leave the host's prompt alone.
///
/// A new run also resets the run-wide abort (a background node started now
/// must not be cancelled on sight by the previous run's abort) and clears the
/// graph when the event's session differs from the last one seen.
pub(crate) fn guide_for_prompt(event: &Value) -> Option<String> {
    let session = event.get("sessionId").and_then(Value::as_str);
    {
        let mut last = LAST_SESSION_ID.lock().unwrap_or_else(|p| p.into_inner());
        if last.as_deref() != session {
            *last = session.map(str::to_string);
            drop(last);
            clear_run_state();
        }
    }
    host_abort_flag().store(false, Ordering::SeqCst);
    if !guide_enabled() {
        return None;
    }
    let base = event
        .get("systemPrompt")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if base.contains(GUIDE_HEADING) {
        return None;
    }
    Some(format!("{base}\n\n{GUIDE_SECTION}"))
}

rpi_plugin_sdk::string_handler! {
/// Append the orchestration guidance to the system prompt, only when opted in.
///
/// Replacement contract: the handler reads the base prompt from the event's
/// `systemPrompt` and writes `{"systemPrompt": "..."}` when it changed it, or
/// `{}` (no change) otherwise. A malformed envelope declines rather than
/// replacing the host's prompt with our section alone.
fn on_before_agent_start(event_json, out, _user_data) {
    let parsed: Value = match serde_json::from_str(event_json) {
        Ok(value) => value,
        Err(_) => {
            write_out(out, serde_json::json!({}));
            return 0;
        }
    };
    match guide_for_prompt(&parsed) {
        Some(next) => write_out(out, serde_json::json!({"systemPrompt": next})),
        None => write_out(out, serde_json::json!({})),
    }
    0
}
}

rpi_plugin_sdk::export_plugin!(|api| {
    if let Some(register_prompt) = api.register_before_agent_start {
        let _ = register_prompt(
            on_before_agent_start,
            rpi_plugin_sdk::callbacks::free_string,
            api.user_data,
        );
    }

    let Some(register) = api.register_tool else {
        return 1;
    };

    {
        let schema = Box::new(crate::kit::schema(
            SUBAGENT_TOOL,
            SUBAGENT_DESCRIPTION,
            SUBAGENT_PARAMS,
        ));
        let rc = register(
            &*schema,
            subagent_tool::execute,
            subagent_tool::poll,
            subagent_tool::cancel,
            subagent_tool::destroy,
            crate::kit::plugin_free_string,
        );
        drop(schema);
        if rc != 0 {
            return 1;
        }
    }

    {
        let schema = Box::new(crate::kit::schema("plan", PLAN_DESCRIPTION, PLAN_PARAMS));
        let rc = register(
            &*schema,
            plan_tool::execute,
            plan_tool::poll,
            plan_tool::cancel,
            plan_tool::destroy,
            crate::kit::plugin_free_string,
        );
        drop(schema);
        if rc != 0 {
            return 1;
        }
    }

    {
        let schema = Box::new(crate::kit::schema(
            "dispatch",
            DISPATCH_DESCRIPTION,
            DISPATCH_PARAMS,
        ));
        let rc = register(
            &*schema,
            dispatch_tool::execute,
            dispatch_tool::poll,
            dispatch_tool::cancel,
            dispatch_tool::destroy,
            crate::kit::plugin_free_string,
        );
        drop(schema);
        if rc != 0 {
            return 1;
        }
    }
    0
});

#[cfg(test)]
mod tests {
    use super::*;

    /// `HOST_ABORT` and the session tracker are process-wide; tests that touch
    /// them must not race. Crate-visible so the `kiqi` face's tests share it.
    pub(crate) static ABORT_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn guide_env_override_is_tri_state() {
        for value in ["1", "true", "TRUE", "yes", "on", " on "] {
            assert_eq!(guide_override(Some(value)), Some(true), "{value}");
        }
        for value in ["0", "false", "no", "off", " OFF "] {
            assert_eq!(guide_override(Some(value)), Some(false), "{value}");
        }
        assert_eq!(guide_override(None), None);
        assert_eq!(guide_override(Some("")), None);
        assert_eq!(guide_override(Some("whatever")), None);
    }

    /// The documented contract: the graph is cleared when the session
    /// switches. The run start tracks the event's `sessionId`; a change wipes
    /// the graph (and cancels running nodes).
    #[test]
    fn a_session_switch_clears_the_graph() {
        // Serializes with the other tests that touch the run-wide abort /
        // call guide_for_prompt.
        let _guard = ABORT_GUARD.lock().unwrap_or_else(|p| p.into_inner());

        guide_for_prompt(&serde_json::json!({"sessionId": "orch-switch-a"}));
        {
            let mut graph = graph_state().lock().unwrap_or_else(|p| p.into_inner());
            let node = crate::graph::Node::from_value(&serde_json::json!({
                "id": "orch-switch-node", "task": "x"
            }))
            .unwrap();
            graph.nodes.push(node);
        }
        assert!(graph_state()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get("orch-switch-node")
            .is_some());

        guide_for_prompt(&serde_json::json!({"sessionId": "orch-switch-b"}));
        assert!(
            graph_state()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get("orch-switch-node")
                .is_none(),
            "the session switch cleared the graph"
        );
    }

    #[test]
    fn active_goal_is_detected_from_the_project_state() {
        assert!(active_goal_from(
            r#"{"active":{"status":"active"},"archived":[]}"#
        ));
        assert!(!active_goal_from(r#"{"active":null,"archived":[]}"#));
        assert!(!active_goal_from(r#"{"active":{"status":"paused"}}"#));
        assert!(!active_goal_from(r#"{"active":{"status":"complete"}}"#));
        assert!(!active_goal_from("{}"));
        assert!(!active_goal_from("not json"));
    }
}
