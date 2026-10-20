//! `kiqi-orchestrator` ported to the wasm ABI (0.7.1).
//!
//! The compile-time plugin runs node workers on OS threads and lets a wave
//! outlive its `dispatch` call. A wasm guest exists only *during* a call, so
//! this port keeps what survives: `plan`'s graph state lives in guest memory
//! across calls (cleared when the session switches, like the compiled
//! version's session tracking), and a wave runs **to completion inside the
//! `dispatch` call** — children run in parallel (the host holds up to the
//! grant's `max-open`), and the guest multiplexes their stdout with per-read
//! timeouts.
//!
//! Semantics that could not survive, stated rather than hidden (README §1–4):
//! no background waves (`waitSeconds: 0` is refused — children die with the
//! call, so "return and collect later" would strand them); no mid-wave
//! reshape (returning mid-wave would kill the in-flight children); no enforced
//! turn ceiling on the child (the env knob has no `proc` equivalent —
//! `maxTurns` is accepted for schema parity but not enforced). The
//! goal-coupled guidance *does* survive: the guest reads `.ki/goals.json`
//! through the project-scoped `fs` grant, so an active goal turns it on as in
//! the compiled plugin. Two compiled conditions are gone: the
//! `KI_ORCHESTRATOR_GUIDE` env override (a guest has no environment), and the
//! subagent guard (`KI_ORCHESTRATOR_SUBAGENT` — a guest's children carry a
//! scrubbed env with no marker, so a delegated child with an active goal still
//! sees the guidance).

use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
use kiqi_plugin_guest::bindings::kiqi::plugin::{exec, fs, proc, types};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ID: &str = "kiqi-orchestrator";
const DEFAULT_PROGRAM: &str = "ki";
const EXCLUDED_TOOLS: &str = "subagent,plan,dispatch";
const MAX_OUTPUT_BYTES: usize = 40_000;
const DEFAULT_MAX_PARALLEL: usize = 4;
const MAX_PARALLEL_CAP: usize = 16;
const RESULT_SUMMARY_BYTES: usize = 2_000;
const POLL_SLICE_MS: u64 = 100;
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;

/// Project-relative path of `kiqi-goal`'s state — the compile-time `GOALS_FILE`.
/// An active goal in it is what turns the guidance on; the `fs` grant's scope is
/// the project tree, so this resolves under the session's directory.
const GOALS_FILE: &str = ".ki/goals.json";
const GUIDE_HEADING: &str = "## Subagent orchestration";

const GUIDE_SECTION: &str = "## Subagent orchestration\n\nFor work that has several steps, or steps that can run in parallel, use the\n`plan` and `dispatch` tools instead of doing everything inline:\n\n1. `plan` with action=set: write the task graph as nodes, each with an `id`, a\n   self-contained `task`, and its `deps`.\n2. `dispatch`: run the current ready wave. Nodes whose dependencies are done run\n   in parallel (up to `maxParallel`).\n3. Read the results, then `plan` with action=update (add/remove/re-dep/reset\n   nodes) and `dispatch` again until the goal is met.\n\nA subagent starts fresh and cannot see this conversation, so each node's `task`\nmust contain everything it needs. Prefer this over hand-rolling the whole\nworkflow with `bash`.\n\nTwo defaults are too tight for a real task and must be raised per node:\n\n- `timeoutSeconds` defaults to **300**. A node that runs a build or the full\n  test suite easily exceeds it, and the call then fails with\n  `subagent timed out after 300s`. Set it explicitly (up to 1800) when the task\ntouches cargo, a browser, or a long script.\n- `maxTurns` defaults to **8** (max 32). Reading code, editing, running gates,\n  checking mutations, and committing does not fit in 8 turns. A node that runs\nthe budget out stops halfway and reports unfinished work as if it were done.\n\nA node that exits early is therefore usually a budget problem, not a logic\nproblem: raise the two numbers before suspecting the task text.";

// ---------------------------------------------------------------------------
// Graph (ported from the compile-time plugin's graph.rs)
// ---------------------------------------------------------------------------

const DEFAULT_TIMEOUT_SECS: u64 = 300;
const MIN_TIMEOUT_SECS: u64 = 10;
const MAX_TIMEOUT_SECS: u64 = 1800;
const MAX_TASK_BYTES: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Skipped => "skipped",
        }
    }
}

#[derive(Clone, Debug)]
struct Node {
    id: String,
    task: String,
    deps: Vec<String>,
    provider: Option<String>,
    model: Option<String>,
    expect_file: Option<String>,
    verify: Option<String>,
    timeout_secs: u64,
    status: Status,
    result: Option<String>,
    reported: bool,
}

fn clamp(value: Option<u64>, default: u64, min: u64, max: u64) -> u64 {
    value.unwrap_or(default).clamp(min, max)
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn parse_string_list(value: Option<&Value>, what: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("{what} must be an array of strings"))?;
    let mut out = Vec::with_capacity(array.len());
    for item in array {
        let text = item
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| format!("{what} must contain non-empty strings"))?;
        out.push(text.to_string());
    }
    Ok(out)
}

impl Node {
    fn from_value(value: &Value) -> Result<Node, String> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or("every node needs a non-empty `id`")?
            .to_string();
        let task = value
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .ok_or_else(|| format!("node `{id}` needs a non-empty `task`"))?
            .to_string();
        if task.len() > MAX_TASK_BYTES {
            return Err(format!("node `{id}` task exceeds {MAX_TASK_BYTES} bytes"));
        }
        let mut deps = parse_string_list(value.get("deps"), &format!("node `{id}` deps"))?;
        let mut seen_deps: Vec<String> = Vec::with_capacity(deps.len());
        deps.retain(|dep| {
            if seen_deps.contains(dep) {
                false
            } else {
                seen_deps.push(dep.clone());
                true
            }
        });
        let timeout_secs = clamp(
            value.get("timeoutSeconds").and_then(Value::as_u64),
            DEFAULT_TIMEOUT_SECS,
            MIN_TIMEOUT_SECS,
            MAX_TIMEOUT_SECS,
        );
        Ok(Node {
            id,
            task,
            deps,
            provider: string_field(value, "provider"),
            model: string_field(value, "model"),
            expect_file: string_field(value, "expectFile"),
            verify: string_field(value, "verify"),
            timeout_secs,
            status: Status::Pending,
            result: None,
            reported: false,
        })
    }

    fn is_done(&self) -> bool {
        self.status == Status::Done
    }

    fn to_value(&self) -> Value {
        json!({
            "id": self.id, "task": self.task, "deps": self.deps,
            "status": self.status.as_str(), "result": self.result,
        })
    }
}

#[derive(Clone, Debug, Default)]
struct Graph {
    nodes: Vec<Node>,
}

impl Graph {
    fn from_nodes_value(value: &Value) -> Result<Graph, String> {
        let array = value.as_array().ok_or("`nodes` must be an array")?;
        if array.is_empty() {
            return Err("`nodes` must contain at least one node".into());
        }
        let mut nodes = Vec::with_capacity(array.len());
        for item in array {
            nodes.push(Node::from_value(item)?);
        }
        let graph = Graph { nodes };
        graph.validate()?;
        Ok(graph)
    }

    fn to_details(&self) -> Value {
        json!({"nodes": self.nodes.iter().map(Node::to_value).collect::<Vec<_>>()})
    }

    fn get(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|node| node.id == id)
    }

    fn has_pending(&self) -> bool {
        self.nodes.iter().any(|node| node.status == Status::Pending)
    }

    /// Pending nodes whose dependencies are all done.
    fn ready(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| node.status == Status::Pending)
            .filter(|node| {
                node.deps
                    .iter()
                    .all(|dep| self.get(dep).map(Node::is_done).unwrap_or(false))
            })
            .map(|node| node.id.clone())
            .collect()
    }

    fn pending_ids(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| node.status == Status::Pending)
            .map(|node| node.id.clone())
            .collect()
    }

    /// Transitively skip every pending node behind a failed/skipped one.
    fn propagate_skips(&mut self) {
        loop {
            let mut changed = false;
            let dead: Vec<String> = self
                .nodes
                .iter()
                .filter(|node| matches!(node.status, Status::Failed | Status::Skipped))
                .map(|node| node.id.clone())
                .collect();
            for node in &mut self.nodes {
                if node.status != Status::Pending {
                    continue;
                }
                if node.deps.iter().any(|dep| dead.contains(dep)) {
                    node.status = Status::Skipped;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    fn validate(&self) -> Result<(), String> {
        let mut seen: Vec<&str> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            if seen.contains(&node.id.as_str()) {
                return Err(format!("duplicate node id `{}`", node.id));
            }
            seen.push(&node.id);
        }
        for node in &self.nodes {
            for dep in &node.deps {
                if !seen.contains(&dep.as_str()) {
                    return Err(format!("node `{}` depends on unknown `{dep}`", node.id));
                }
            }
        }
        // Kahn's algorithm over the dependency edges.
        let mut indegree: Vec<usize> = self.nodes.iter().map(|node| node.deps.len()).collect();
        let mut queue: Vec<usize> = (0..self.nodes.len())
            .filter(|index| indegree[*index] == 0)
            .collect();
        let mut visited = 0usize;
        while let Some(index) = queue.pop() {
            visited += 1;
            let id = self.nodes[index].id.clone();
            for (other, node) in self.nodes.iter().enumerate() {
                if node.deps.contains(&id) {
                    indegree[other] -= 1;
                    if indegree[other] == 0 {
                        queue.push(other);
                    }
                }
            }
        }
        if visited == self.nodes.len() {
            Ok(())
        } else {
            let cyclic: Vec<String> = self
                .nodes
                .iter()
                .enumerate()
                .filter(|(index, _)| indegree[*index] > 0)
                .map(|(_, node)| node.id.clone())
                .collect();
            Err(format!("cycle detected among: {}", cyclic.join(", ")))
        }
    }

    fn render(&self) -> String {
        if self.nodes.is_empty() {
            return "plan is empty".into();
        }
        let done = self
            .nodes
            .iter()
            .filter(|n| n.status == Status::Done)
            .count();
        let mut lines = vec![format!("{done}/{} done", self.nodes.len())];
        for node in &self.nodes {
            let mark = match node.status {
                Status::Done => "x",
                Status::Failed => "!",
                Status::Skipped => "-",
                Status::Running => "~",
                Status::Pending => " ",
            };
            let deps = if node.deps.is_empty() {
                String::new()
            } else {
                format!(" deps=[{}]", node.deps.join(","))
            };
            lines.push(format!(
                "[{mark}] {} {}{deps}",
                node.id,
                node.status.as_str()
            ));
        }
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Guest state: statics persist across calls within a session, like the
// compiled plugin's process statics
// ---------------------------------------------------------------------------

static GRAPH: Mutex<Graph> = Mutex::new(Graph { nodes: Vec::new() });
/// `(node id, child handle, when that node's own timeout expires)`.
static RUNNING: Mutex<Vec<(String, u32, Instant)>> = Mutex::new(Vec::new());
static GENERATION: AtomicU64 = AtomicU64::new(0);
static LAST_SESSION_ID: Mutex<Option<String>> = Mutex::new(None);

fn graph_state() -> &'static Mutex<Graph> {
    &GRAPH
}

fn running() -> &'static Mutex<Vec<(String, u32, Instant)>> {
    &RUNNING
}

fn bump_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Whether `kiqi-goal` has an active goal in this project — the compile-time
/// `active_goal_in_project` gate, reading `.ki/goals.json` through the `fs`
/// grant. Fail-closed: any read or parse problem is "no goal".
fn active_goal_in_project() -> bool {
    let Ok(bytes) = fs::read(GOALS_FILE) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(active) = value.get("active") else {
        return false;
    };
    !active.is_null() && active.get("status").and_then(Value::as_str) == Some("active")
}

/// Session switch → cancel running children, drop the graph. Same contract as
/// the compiled plugin's `note_session`.
fn note_session(event: &Value) {
    let session = event.get("sessionId").and_then(Value::as_str);
    let mut last = LAST_SESSION_ID.lock().unwrap_or_else(|p| p.into_inner());
    if last.as_deref() != session {
        *last = session.map(str::to_string);
        drop(last);
        stop_all_running();
        let mut graph = graph_state().lock().unwrap_or_else(|p| p.into_inner());
        *graph = Graph::default();
        drop(graph);
        bump_generation();
    }
}

fn stop_all_running() {
    let handles: Vec<u32> = running()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|(_, handle, _)| *handle)
        .collect();
    for handle in handles {
        let _ = proc::kill(handle);
    }
    running().lock().unwrap_or_else(|p| p.into_inner()).clear();
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

fn plan(params: &Value) -> Result<types::ToolResult, String> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("show");
    let mut guard = graph_state().lock().unwrap_or_else(|p| p.into_inner());
    match action {
        "set" => {
            let nodes = params
                .get("nodes")
                .ok_or("`nodes` is required for action=set")?;
            let graph = Graph::from_nodes_value(nodes)?;
            // A new plan replaces the work in flight: kill running children and
            // invalidate anything else in flight.
            stop_all_running();
            bump_generation();
            *guard = graph;
        }
        "update" => {
            let mut next = guard.clone();
            apply_update(&mut next, params)?;
            next.validate()?;
            *guard = next;
        }
        "show" => {}
        other => {
            return Err(format!(
                "unknown plan action `{other}` (expected set, update, show)"
            ))
        }
    }
    Ok(result_with_details(&guard.render(), guard.to_details()))
}

fn apply_update(graph: &mut Graph, params: &Value) -> Result<(), String> {
    if let Some(add) = params.get("addNodes") {
        let array = add.as_array().ok_or("`addNodes` must be an array")?;
        for item in array {
            let node = Node::from_value(item)?;
            if graph.get(&node.id).is_some() {
                return Err(format!("node `{}` already exists", node.id));
            }
            graph.nodes.push(node);
        }
    }
    if let Some(remove) = params.get("removeIds") {
        let array = remove.as_array().ok_or("`removeIds` must be an array")?;
        for item in array {
            let id = item
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or("`removeIds` must contain non-empty strings")?;
            cancel_node(graph, id);
            graph.nodes.retain(|node| node.id != id);
        }
    }
    if let Some(cancel) = params.get("cancel") {
        let array = cancel.as_array().ok_or("`cancel` must be an array")?;
        for item in array {
            let id = item
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or("`cancel` must contain non-empty strings")?;
            cancel_node(graph, id);
        }
    }
    if let Some(set_deps) = params.get("setDeps") {
        let array = set_deps.as_array().ok_or("`setDeps` must be an array")?;
        for item in array {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or("`setDeps` entries need non-empty `id`s")?
                .to_string();
            let mut parsed = parse_string_list(item.get("deps"), "`deps`")?;
            let mut seen: Vec<String> = Vec::with_capacity(parsed.len());
            parsed.retain(|dep| {
                if seen.contains(dep) {
                    false
                } else {
                    seen.push(dep.clone());
                    true
                }
            });
            let node = graph
                .get_mut(&id)
                .ok_or_else(|| format!("node `{id}` not found"))?;
            node.deps = parsed;
        }
    }
    if let Some(reset) = params.get("reset") {
        let array = reset.as_array().ok_or("`reset` must be an array")?;
        for item in array {
            let id = item
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or("`reset` must contain non-empty strings")?;
            let node = graph
                .get_mut(id)
                .ok_or_else(|| format!("node `{id}` not found"))?;
            if node.status == Status::Running {
                return Err(format!(
                    "node `{id}` is running; cancel it before resetting"
                ));
            }
            node.status = Status::Pending;
            node.result = None;
            node.reported = false;
        }
    }
    Ok(())
}

/// A running node is killed through its handle; a pending one is skipped (and
/// its dependents follow), same as the compiled version.
fn cancel_node(graph: &mut Graph, id: &str) {
    let handle = running()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .find(|(node_id, _, _)| node_id == id)
        .map(|(_, handle, _)| *handle);
    if let Some(handle) = handle {
        let _ = proc::kill(handle);
        if let Some(node) = graph.get_mut(id) {
            node.status = Status::Failed;
            node.result = Some("cancelled".into());
        }
        return;
    }
    if let Some(node) = graph.get_mut(id) {
        if node.status == Status::Pending {
            node.status = Status::Skipped;
            graph.propagate_skips();
        }
    }
}

// ---------------------------------------------------------------------------
// Tool surface
// ---------------------------------------------------------------------------

fn parameters(properties: Map<String, Value>, required: &[&str]) -> String {
    let mut schema = json!({"type":"object", "properties": properties});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema.to_string()
}

fn tools() -> Vec<types::ToolSpec> {
    vec![
        types::ToolSpec {
            name: "subagent".to_string(),
            description: "Run one bounded task in a fresh child agent process and return its final answer.".to_string(),
            parameters_schema: parameters(json!({
                "task":{"type":"string","maxLength":20000},
                "program":{"type":"string","description":"Child program; default ki (resolved from PATH)"},
                "provider":{"type":"string"},"model":{"type":"string"},
                "maxTurns":{"type":"integer","minimum":1,"maximum":32},
                "timeoutSeconds":{"type":"integer","minimum":10,"maximum":1800}
            }).as_object().unwrap().clone(), &["task"]),
        },
        types::ToolSpec {
            name: "plan".to_string(),
            description: "Author or edit the task DAG. action=set (replace with `nodes`), action=update (addNodes/removeIds/cancel/setDeps/reset), action=show.".to_string(),
            parameters_schema: parameters(json!({
                "action":{"type":"string","enum":["set","update","show"]},
                "nodes":{"type":"array","description":"action=set: full node list"},
                "addNodes":{"type":"array"},"removeIds":{"type":"array","items":{"type":"string"}},
                "cancel":{"type":"array","items":{"type":"string"}},
                "setDeps":{"type":"array"},
                "reset":{"type":"array","items":{"type":"string"}}
            }).as_object().unwrap().clone(), &["action"]),
        },
        types::ToolSpec {
            name: "dispatch".to_string(),
            description: "Schedule the plan: start every ready node within maxParallel and wait for the wave to finish. In the wasm port the wave cannot outlive this call, so waitSeconds:0 is refused; waitSeconds:N kills and fails whatever is still running after N seconds.".to_string(),
            parameters_schema: parameters(json!({
                "maxParallel":{"type":"integer","minimum":1,"maximum":16},
                "waitSeconds":{"type":"integer","minimum":0},
                "program":{"type":"string","description":"Child program; default ki (resolved from PATH)"}
            }).as_object().unwrap().clone(), &[]),
        },
    ]
}

fn result_with_details(text: &str, details: Value) -> types::ToolResult {
    types::ToolResult {
        blocks: vec![types::ContentBlock::Text(text.to_string())],
        is_error: false,
        details: details.to_string(),
    }
}

fn summarize(text: &str) -> String {
    if text.len() <= RESULT_SUMMARY_BYTES {
        return text.to_string();
    }
    let mut end = RESULT_SUMMARY_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn truncate_output(mut text: String) -> String {
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

fn capability_reason(error: types::CapabilityError) -> String {
    match error {
        types::CapabilityError::Denied(reason) => reason,
        types::CapabilityError::Failed(reason) => format!("failed:{reason}"),
    }
}

// ---------------------------------------------------------------------------
// subagent: one blocking child on `proc`
// ---------------------------------------------------------------------------

fn run_subagent(params: &Value) -> Result<types::ToolResult, String> {
    let task = params
        .get("task")
        .and_then(Value::as_str)
        .ok_or("missing string parameter `task`")?
        .trim()
        .to_string();
    if task.is_empty() {
        return Err("task must not be empty".into());
    }
    if task.len() > MAX_TASK_BYTES {
        return Err(format!("task must be at most {MAX_TASK_BYTES} bytes"));
    }
    // Recorded for schema parity; on wasm the child's turn ceiling has no env
    // knob, so it is not enforced (README, difference 3).
    let max_turns = clamp(params.get("maxTurns").and_then(Value::as_u64), 8, 1, 32);
    let timeout_secs = clamp(
        params.get("timeoutSeconds").and_then(Value::as_u64),
        300,
        10,
        1800,
    );
    let program = string_field(params, "program").unwrap_or_else(|| DEFAULT_PROGRAM.to_string());
    // Only the default `ki` gets ki-specific flags; an overridden program
    // (the hermetic seam) receives nothing but the task on stdin.
    let is_ki = program == DEFAULT_PROGRAM;
    let mut args: Vec<String> = if is_ki {
        vec![
            "-p".into(),
            "--no-session".into(),
            "--exclude-tools".into(),
            EXCLUDED_TOOLS.into(),
        ]
    } else {
        Vec::new()
    };
    if is_ki {
        if let Some(provider) = string_field(params, "provider") {
            args.push("--provider".into());
            args.push(provider);
        }
        if let Some(model) = string_field(params, "model") {
            args.push("--model".into());
            args.push(model);
        }
    }

    // Esc before we even start: do not spawn a child this call would have to
    // abandon. Same guard (and message) as `dispatch`.
    if proc::cancel_requested() {
        return Err("cancelled by the host".into());
    }

    let handle = proc::spawn(&program, &args, None).map_err(|error| match error {
        types::CapabilityError::Denied(reason) => format!(
            "{reason} — the subagent child needs `ki plugin grant {ID} proc \\
             --command {program} --cwd project --timeout 1800`"
        ),
        types::CapabilityError::Failed(reason) => {
            format!("failed to start subagent `{program}`: {reason}")
        }
    })?;
    // The task rides stdin; EOF tells the child the prompt is complete.
    if let Err(error) = proc::write(handle, task.as_bytes()) {
        let _ = proc::kill(handle);
        return Err(format!(
            "failed to write the task: {}",
            capability_reason(error)
        ));
    }
    let _ = proc::close_stdin(handle);

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut output = String::new();
    loop {
        // Esc while the child runs: kill it and return instead of waiting out
        // `timeoutSeconds` (up to 1800 s) while this call holds the plugin's
        // call mutex. `read_line` slices at POLL_SLICE_MS, so a cancel is
        // noticed within ~100 ms; the host reaps the group on call return and
        // `kill` is idempotent.
        if proc::cancel_requested() {
            let _ = proc::kill(handle);
            return Err("cancelled by the host".into());
        }
        match proc::read_line(handle, MAX_LINE_BYTES, POLL_SLICE_MS) {
            Ok(Some(line)) => {
                output.push_str(&line);
                output.push('\n');
            }
            // stdout ended: collect the exit status.
            Ok(None) => break,
            // No output in this slice: keep polling until the budget runs out.
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            let _ = proc::kill(handle);
            return Err(format!("subagent timed out after {timeout_secs}s"));
        }
    }
    let status = match proc::wait(handle, 2_000) {
        Ok(Some(code)) => code,
        Ok(None) => {
            // stdout is at EOF, so the child has finished producing; one that
            // lingers past the grace window (flushing its session, say) is
            // still finished — kill the leftover and score the complete
            // output, instead of inventing a failure exit of -1.
            let _ = proc::kill(handle);
            0
        }
        Err(error) => return Err(capability_reason(error)),
    };
    let text = truncate_output(output);
    if status != 0 {
        return Err(format!("subagent exited with {status}:\n{}", text.trim()));
    }
    // A zero exit with nothing on stdout is not a result this tool can use. A
    // truncated run now exits non-zero (a budget stop maps to exit 1), so this is
    // a backstop for a child that succeeds yet prints nothing at all: `ki -p`
    // prints its answer once, at the end of the run, so empty stdout means no
    // final message was ever produced. The compile-time channel
    // (`plugins/kiqi-orchestrator/src/spawn.rs`) rejects the same shape — keep
    // both channels in step.
    if text.trim().is_empty() {
        return Err(format!(
            "subagent exited successfully but produced no output; the run may have \
             hit its turn budget (maxTurns={max_turns})."
        ));
    }
    Ok(types::ToolResult {
        blocks: vec![types::ContentBlock::Text(text.trim().to_string())],
        is_error: false,
        details: "null".to_string(),
    })
}

// ---------------------------------------------------------------------------
// dispatch: a wave that runs to completion inside the call
// ---------------------------------------------------------------------------

fn dispatch(params: &Value) -> Result<types::ToolResult, String> {
    let max_parallel = clamp(
        params.get("maxParallel").and_then(Value::as_u64),
        DEFAULT_MAX_PARALLEL as u64,
        1,
        MAX_PARALLEL_CAP as u64,
    ) as usize;
    let wait_seconds = params.get("waitSeconds").and_then(Value::as_u64);
    // The program override is the hermetic seam (a test drives the wave with
    // `cat`); in production it stays `ki`, resolved from PATH.
    let wave_program =
        string_field(params, "program").unwrap_or_else(|| DEFAULT_PROGRAM.to_string());
    if wait_seconds == Some(0) {
        return Err(
            "waitSeconds:0 (background waves) is not available on the wasm tier: children \
             cannot outlive this call, so dispatch always waits for the wave"
                .into(),
        );
    }
    // Esc/abort: the host cancels the call. Check before spawning anything and
    // once per wave poll below; the host still reaps every child on return.
    if proc::cancel_requested() {
        return Err("cancelled by the host".into());
    }

    // ---- Start the ready set within the concurrency budget. ----
    let started = {
        let mut graph = graph_state().lock().unwrap_or_else(|p| p.into_inner());
        if graph.nodes.is_empty() {
            return Err("no plan yet: call plan(action=set, nodes=[...]) first".into());
        }
        let running_now = running().lock().unwrap_or_else(|p| p.into_inner()).len();
        let budget = max_parallel.saturating_sub(running_now);
        let mut ready = graph.ready();
        if ready.is_empty() && running_now == 0 && graph.has_pending() {
            let blocked = graph.pending_ids().join(", ");
            return Err(format!(
                "no progress: no pending node is ready (blocked by unmet dependencies): {blocked}"
            ));
        }
        ready.truncate(budget);
        let mut started = Vec::with_capacity(ready.len());
        for id in ready {
            let Some(node) = graph.get(&id).cloned() else {
                continue;
            };
            let handle = match spawn_node(&node, &wave_program) {
                Ok(handle) => handle,
                Err(problem) => {
                    // The node fails immediately (and its dependents skip) —
                    // the message names the spawn problem or the missing grant.
                    if let Some(slot) = graph.get_mut(&id) {
                        slot.status = Status::Failed;
                        slot.result = Some(summarize(&problem));
                    }
                    graph.propagate_skips();
                    continue;
                }
            };
            // The task rides stdin; EOF tells the child the prompt is complete.
            if let Err(error) = proc::write(handle, node.task.as_bytes()) {
                let _ = proc::kill(handle);
                if let Some(slot) = graph.get_mut(&id) {
                    slot.status = Status::Failed;
                    slot.result = Some(format!(
                        "failed to write the task: {}",
                        capability_reason(error)
                    ));
                }
                graph.propagate_skips();
                continue;
            }
            let _ = proc::close_stdin(handle);
            if let Some(slot) = graph.get_mut(&id) {
                slot.status = Status::Running;
            }
            // The node's own timeout bounds its child (the host kills the whole
            // group at call end; the guest enforces the node-level deadline).
            let deadline = Instant::now() + Duration::from_secs(node.timeout_secs);
            running().lock().unwrap_or_else(|p| p.into_inner()).push((
                id.clone(),
                handle,
                deadline,
            ));
            started.push(id);
        }
        started
    };

    // ---- The wave runs to completion inside this call. ----
    // `waitSeconds` bounds the whole wave; whatever is still running when it
    // passes is killed with its process group and the node fails (children
    // cannot outlive the call, so "leave them running" is not an option).
    let wave_deadline = wait_seconds.map(|secs| Instant::now() + Duration::from_secs(secs));
    let mut outputs: HashMap<u32, String> = HashMap::new();
    'wave: loop {
        if proc::cancel_requested() {
            fail_remaining("cancelled by the host");
            break 'wave;
        }
        // Poll every live child each round. Polling only the head of the list
        // strands siblings: one that finishes while an earlier node is still
        // streaming would never be read or collected, and the `waitSeconds`
        // expiry would then kill it and mark a successfully finished node
        // failed.
        let batch: Vec<(String, u32, Instant)> =
            running().lock().unwrap_or_else(|p| p.into_inner()).clone();
        if batch.is_empty() {
            break;
        }
        for (id, handle, deadline) in batch {
            // A sibling handled earlier in this round may already have left
            // the list (collected at EOF, or timed out and killed).
            if !still_running(handle) {
                continue;
            }
            if Instant::now() >= deadline {
                let _ = proc::kill(handle);
                let mut graph = graph_state().lock().unwrap_or_else(|p| p.into_inner());
                if let Some(slot) = graph.get_mut(&id) {
                    slot.status = Status::Failed;
                    slot.result = Some("timed out".into());
                }
                graph.propagate_skips();
                running()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retain(|(node_id, _, _)| node_id != &id);
                continue;
            }
            match proc::read_line(handle, MAX_LINE_BYTES, POLL_SLICE_MS) {
                Ok(Some(line)) => {
                    let buffer = outputs.entry(handle).or_default();
                    buffer.push_str(&line);
                    buffer.push('\n');
                }
                Ok(None) => {
                    // EOF: the child has stopped producing. Give it a short
                    // grace to exit; one that lingers past it (flushing its
                    // session, say) is still finished as far as this wave is
                    // concerned, so kill the leftover and score its complete
                    // output — a bare `-1` here used to fail finished nodes.
                    let exit = match proc::wait(handle, 2_000) {
                        Ok(Some(code)) => code,
                        Ok(None) | Err(_) => {
                            let _ = proc::kill(handle);
                            0
                        }
                    };
                    let mut graph = graph_state().lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(slot) = graph.get_mut(&id) {
                        let output = outputs.get(&handle).cloned().unwrap_or_default();
                        let verified = if exit == 0 {
                            verify_node(slot)
                        } else {
                            Err(String::new())
                        };
                        match (exit, verified) {
                            (0, Ok(())) => {
                                slot.status = Status::Done;
                                slot.result = Some(summarize(&truncate_output(output)));
                            }
                            (0, Err(problem)) => {
                                slot.status = Status::Failed;
                                slot.result = Some(summarize(&problem));
                            }
                            _ => {
                                slot.status = Status::Failed;
                                slot.result = Some(summarize(&format!("{output}\n[exit {exit}]")));
                            }
                        }
                    }
                    graph.propagate_skips();
                    running()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .retain(|(node_id, _, _)| node_id != &id);
                }
                Err(_) => {}
            }
            if wave_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                fail_remaining("the dispatch wave hit its waitSeconds bound");
                break 'wave;
            }
        }
    }

    // ---- Collect. ----
    let graph = graph_state().lock().unwrap_or_else(|p| p.into_inner());
    let running_ids: Vec<String> = running()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect();
    let mut text = format!(
        "started {} · running {}\n",
        started.len(),
        running_ids.len()
    );
    if !started.is_empty() {
        text.push_str(&format!("started: {}\n", started.join(", ")));
    }
    if !running_ids.is_empty() {
        text.push_str(&format!("running: {}\n", running_ids.join(", ")));
    }
    text.push('\n');
    text.push_str(&graph.render());
    Ok(result_with_details(&text, graph.to_details()))
}

/// Whether `handle` is still in the running list (a sibling earlier in the
/// same polling round may have collected or killed it already).
fn still_running(handle: u32) -> bool {
    running()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|(_, open, _)| *open == handle)
}

/// Kill and fail whatever is still running when the wave's patience runs out.
fn fail_remaining(reason: &str) {
    let handles: Vec<u32> = running()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .map(|(_, handle, _)| *handle)
        .collect();
    for handle in handles {
        let _ = proc::kill(handle);
    }
    let mut graph = graph_state().lock().unwrap_or_else(|p| p.into_inner());
    for node in &mut graph.nodes {
        if node.status == Status::Running {
            node.status = Status::Failed;
            node.result = Some(reason.to_string());
        }
    }
    graph.propagate_skips();
    running().lock().unwrap_or_else(|p| p.into_inner()).clear();
}

fn spawn_node(node: &Node, program: &str) -> Result<u32, String> {
    // Same rule as run_subagent: ki-specific flags only for the default ki.
    let mut args: Vec<String> = if program == DEFAULT_PROGRAM {
        vec![
            "-p".into(),
            "--no-session".into(),
            "--exclude-tools".into(),
            EXCLUDED_TOOLS.into(),
        ]
    } else {
        Vec::new()
    };
    if program == DEFAULT_PROGRAM {
        if let Some(provider) = &node.provider {
            args.push("--provider".into());
            args.push(provider.clone());
        }
        if let Some(model) = &node.model {
            args.push("--model".into());
            args.push(model.clone());
        }
    }
    proc::spawn(program, &args, None).map_err(|error| match error {
        types::CapabilityError::Denied(reason) => format!(
            "{reason} — the node children need `ki plugin grant {ID} proc \\
             --command {program} --cwd project --timeout 1800`"
        ),
        types::CapabilityError::Failed(reason) => reason,
    })
}

/// The compiled dispatch's acceptance checks, over `exec`: `expectFile` must
/// exist and `verify` must exit 0. Both need `sh`/`test` in the exec
/// allow-list (a difference the guest README documents) — without them the
/// node fails honestly instead of "succeeding" without its output.
fn verify_node(node: &Node) -> Result<(), String> {
    if let Some(file) = &node.expect_file {
        let raw = exec::run("test", &["-f".to_string(), file.clone()], None)
            .map_err(|error| format!("test -f {file}: {}", capability_reason(error)))?;
        let status = parse_status(&raw);
        if status != Some(0) {
            return Err(format!(
                "expect_file `{file}` is missing after the child ran"
            ));
        }
    }
    if let Some(command) = &node.verify {
        let raw = exec::run("sh", &["-c".to_string(), command.clone()], None)
            .map_err(|error| format!("verify: {}", capability_reason(error)))?;
        let output = parse_exec(&raw)?;
        if output.status != Some(0) {
            return Err(format!("verify failed: {}", output.stderr.trim()));
        }
    }
    Ok(())
}

fn parse_status(raw: &str) -> Option<i64> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|parsed| parsed.get("status").and_then(Value::as_i64))
}

fn parse_exec(raw: &str) -> Result<ExecOutput, String> {
    let parsed: Value =
        serde_json::from_str(raw).map_err(|error| format!("bad capability response: {error}"))?;
    Ok(ExecOutput {
        status: parsed.get("status").and_then(Value::as_i64),
        stderr: parsed
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

struct ExecOutput {
    status: Option<i64>,
    stderr: String,
}

// ---------------------------------------------------------------------------
// The Guest impl
// ---------------------------------------------------------------------------

struct KiOrchestrator;

impl Guest for KiOrchestrator {
    fn id() -> String {
        ID.to_string()
    }

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        tools()
    }

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
        if !tools().iter().any(|spec| spec.name == name) {
            return Err(types::PluginError::UnknownTool(name));
        }
        let params: Value = serde_json::from_str(&arguments)
            .map_err(|error| types::PluginError::InvalidArguments(error.to_string()))?;
        match name.as_str() {
            "subagent" => run_subagent(&params).map(|result| result),
            "plan" => plan(&params),
            "dispatch" => dispatch(&params),
            other => Err(other.to_string()),
        }
        .map_err(types::PluginError::Failed)
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        Vec::new()
    }

    fn run_command(_name: String, _arguments: String) -> Result<String, types::PluginError> {
        Err(types::PluginError::UnknownTool(_name))
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        Vec::new()
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        Vec::new()
    }

    fn on_event(_event: types::Event) -> types::EventAction {
        types::EventAction::Continue
    }

    fn before_agent_start(prompt: String) -> Option<String> {
        let event: Value = serde_json::from_str(&prompt).ok()?;
        note_session(&event);
        // The compile-time gate: guidance only when the project has an active
        // goal. The compiled env override (`KI_ORCHESTRATOR_GUIDE`) is gone —
        // a guest has no environment to read — so the file alone decides.
        if !active_goal_in_project() {
            return None;
        }
        let base = event
            .get("systemPrompt")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // Idempotent, like the compiled handler: never append the section twice.
        if base.contains(GUIDE_HEADING) {
            return None;
        }
        Some(format!("{base}\n\n{GUIDE_SECTION}"))
    }

    fn list_probe_points() -> Vec<String> {
        Vec::new()
    }

    fn probe(_point: String, _payload: String) -> types::Verdict {
        types::Verdict::Continue
    }
}

kiqi_plugin_guest::bindings::export!(KiOrchestrator with_types_in kiqi_plugin_guest::bindings);
