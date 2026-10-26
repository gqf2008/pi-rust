//! `kiqi-todo` ported to the wasm ABI (0.3.0).
//!
//! Same semantics as the compile-time extension, but the two channels the port is
//! meant to validate are the ABI's instead of `HostCaps`:
//!
//! * state rides in the structured `tool-result.details` (the compile-time
//!   extension puts its whole list in `ToolOutcome.details`);
//! * the list is restored from `session-read.session-branch` on
//!   `SessionStart` / `SessionTree`, not from a runtime action.

use std::sync::{Mutex, OnceLock};

use kiqi_wasm_bindings::bindings::exports::kiqi::extension::extension_api::Guest;
use kiqi_wasm_bindings::bindings::kiqi::extension::{session_read, types};
use serde_json::{json, Value};

const TOOL: &str = "todo";
const COMMAND: &str = "todos";
const DESCRIPTION: &str = "Manage one session task list. Actions: list, add (text), update (id, status: pending/in_progress/completed), toggle (id), clear. Mark the task you are working on in_progress and completed when finished.";
const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["list","add","update","toggle","clear"]},"text":{"type":"string"},"id":{"type":"integer"},"status":{"type":"string","enum":["pending","in_progress","completed"]},"includeDone":{"type":"boolean"}},"required":["action"]}"#;

#[derive(Clone, Debug, Default)]
struct TodoState {
    todos: Vec<Todo>,
    next_id: u64,
}

#[derive(Clone, Debug)]
struct Todo {
    id: u64,
    text: String,
    done: bool,
    in_progress: bool,
}

fn state() -> &'static Mutex<TodoState> {
    static STATE: OnceLock<Mutex<TodoState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(TodoState::default()))
}

fn parse_todos(value: &Value) -> Option<TodoState> {
    let todos = value.get("todos")?.as_array()?;
    let mut result = Vec::new();
    let mut next_id = value.get("nextId").and_then(Value::as_u64).unwrap_or(1);
    for item in todos {
        let id = item.get("id").and_then(Value::as_u64)?;
        let text = item.get("text").and_then(Value::as_str)?.to_string();
        result.push(Todo {
            id,
            text,
            done: item
                .get("done")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| item.get("status").and_then(Value::as_str) == Some("completed")),
            in_progress: item.get("status").and_then(Value::as_str) == Some("in_progress"),
        });
        next_id = next_id.max(id.saturating_add(1));
    }
    Some(TodoState {
        todos: result,
        next_id,
    })
}

fn state_json(state: &TodoState) -> Value {
    json!({
        "todos": state.todos.iter().map(|todo| json!({
            "id": todo.id,
            "text": todo.text,
            "done": todo.done,
            "status": if todo.done { "completed" } else if todo.in_progress { "in_progress" } else { "pending" },
        })).collect::<Vec<_>>(),
        "nextId": state.next_id,
    })
}

fn render(state: &TodoState, include_done: bool) -> String {
    if state.todos.is_empty() {
        return "No todos".into();
    }
    let done = state.todos.iter().filter(|todo| todo.done).count();
    let mut lines = vec![format!("{done}/{} completed", state.todos.len())];
    for todo in state.todos.iter().filter(|todo| include_done || !todo.done) {
        lines.push(format!(
            "[{}] #{}: {}",
            if todo.done {
                "✓"
            } else if todo.in_progress {
                "•"
            } else {
                " "
            },
            todo.id,
            todo.text
        ));
    }
    lines.join("\n")
}

fn result(action: &str, state: &TodoState, text: String, error: Option<&str>) -> types::ToolResult {
    let mut details = state_json(state);
    details["action"] = json!(action);
    if let Some(error) = error {
        details["error"] = json!(error);
    }
    types::ToolResult {
        blocks: vec![types::ContentBlock::Text(text)],
        is_error: error.is_some(),
        details: details.to_string(),
    }
}

fn execute(params: &Value) -> Result<types::ToolResult, types::ExtensionError> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("list");
    let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
    match action {
        "list" => Ok(result(
            action,
            &guard,
            render(
                &guard,
                params
                    .get("includeDone")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            None,
        )),
        "add" => {
            let text = params
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if text.is_empty() {
                return Ok(result(
                    action,
                    &guard,
                    "Error: text required for add".into(),
                    Some("text required"),
                ));
            }
            let todo = Todo {
                id: guard.next_id.max(1),
                text: text.to_string(),
                done: false,
                in_progress: false,
            };
            guard.next_id = todo.id.saturating_add(1);
            guard.todos.push(todo);
            Ok(result(action, &guard, render(&guard, true), None))
        }
        "toggle" => {
            let id = params
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("id required for toggle"))?;
            let Some(index) = guard.todos.iter().position(|todo| todo.id == id) else {
                return Ok(result(
                    action,
                    &guard,
                    format!("Todo #{id} not found"),
                    Some("todo not found"),
                ));
            };
            guard.todos[index].done = !guard.todos[index].done;
            guard.todos[index].in_progress = false;
            Ok(result(action, &guard, render(&guard, true), None))
        }
        "update" => {
            let id = params
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("id required for update"))?;
            let status = params
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("status required for update"))?;
            if !matches!(status, "pending" | "in_progress" | "completed") {
                return Err(invalid("status must be pending, in_progress, or completed"));
            }
            let Some(item) = guard.todos.iter_mut().find(|todo| todo.id == id) else {
                return Ok(result(
                    action,
                    &guard,
                    format!("Todo #{id} not found"),
                    Some("todo not found"),
                ));
            };
            item.done = status == "completed";
            item.in_progress = status == "in_progress";
            Ok(result(action, &guard, render(&guard, true), None))
        }
        "clear" => {
            guard.todos.clear();
            guard.next_id = 1;
            Ok(result(action, &guard, render(&guard, true), None))
        }
        other => Err(invalid(&format!(
            "action must be one of: list, add, update, toggle, clear (got `{other}`)"
        ))),
    }
}

fn invalid(detail: &str) -> types::ExtensionError {
    types::ExtensionError::InvalidArguments(detail.to_string())
}

/// Rebuild the list from the session branch, the same walk the compile-time
/// extension does — but the branch arrives through `session-read`.
fn restore() {
    let branch: Value =
        serde_json::from_str(&session_read::session_branch()).unwrap_or(Value::Null);
    let mut restored = TodoState::default();
    if let Some(entries) = branch.get("entries").and_then(Value::as_array) {
        for entry in entries {
            let Some(message) = entry.get("message") else {
                continue;
            };
            if message.get("role").and_then(Value::as_str) != Some("toolResult")
                || message.get("toolName").and_then(Value::as_str) != Some(TOOL)
            {
                continue;
            }
            if let Some(next) = parse_todos(message.get("details").unwrap_or(&Value::Null)) {
                restored = next;
            }
        }
    }
    *state().lock().unwrap_or_else(|p| p.into_inner()) = restored;
}

struct TodoExtension;

impl Guest for TodoExtension {
    /// No selector/editor/prompt flow exists in this extension yet; a reply is
    /// refused rather than silently dropped.
    fn continue_command(
        _name: String,
        _flow_id: String,
        _reply: types::CommandReply,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        Err(types::ExtensionError::Failed(
            "this extension declares no command continuations".to_string(),
        ))
    }

    fn id() -> String {
        "kiqi-todo".to_string()
    }

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        vec![types::ToolSpec {
            name: TOOL.to_string(),
            description: DESCRIPTION.to_string(),
            parameters_schema: SCHEMA.to_string(),
        }]
    }

    fn call_tool(
        name: String,
        arguments: String,
    ) -> Result<types::ToolResult, types::ExtensionError> {
        if name != TOOL {
            return Err(types::ExtensionError::UnknownTool(name));
        }
        let params: Value = serde_json::from_str(&arguments).unwrap_or_else(|_| json!({}));
        execute(&params)
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        vec![types::CommandSpec {
            name: COMMAND.to_string(),
            description: "Show todos on the current session branch".to_string(),
        }]
    }

    fn run_command(
        name: String,
        arguments: String,
    ) -> Result<types::CommandResult, types::ExtensionError> {
        if name != COMMAND {
            return Err(types::ExtensionError::UnknownTool(name));
        }
        // `flag:<name>` exists so the host test can prove declaration-scoped
        // `cli-flag`: this guest declares `todo-flag` and nothing else.
        if let Some(flag) = arguments.strip_prefix("flag:") {
            let value = match session_read::cli_flag(flag) {
                None => "none".to_string(),
                Some(types::FlagValue::Bare) => "bare".to_string(),
                Some(types::FlagValue::Text(text)) => format!("text:{text}"),
            };
            return Ok(types::CommandResult {
                flow_id: String::new(),
                body: types::CommandResultBody::Message(format!("flag={value}")),
            });
        }
        Ok(types::CommandResult {
            flow_id: String::new(),
            body: types::CommandResultBody::Message(render(
                &state().lock().unwrap_or_else(|p| p.into_inner()),
                true,
            )),
        })
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        vec![types::FlagSpec {
            name: "todo-flag".to_string(),
            description: "Todo flag".to_string(),
        }]
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        Vec::new()
    }

    fn on_event(event: types::Event) -> types::EventAction {
        if matches!(
            event,
            types::Event::SessionStart | types::Event::SessionTree
        ) {
            restore();
        }
        types::EventAction::Continue
    }

    fn before_agent_start(_prompt: String) -> Option<String> {
        None
    }

    fn list_probe_points() -> Vec<String> {
        Vec::new()
    }

    fn probe(_point: String, _payload: String) -> types::Verdict {
        types::Verdict::Continue
    }
}

kiqi_wasm_bindings::bindings::export!(TodoExtension with_types_in kiqi_wasm_bindings::bindings);

// The 0.8.0 contribution exports are mandatory world exports; deny them all.
kiqi_wasm_bindings::deny_contribution_exports!(TodoExtension);
