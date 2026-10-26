//! Plan-mode logic for the `kiqi-plan-mode` wasm guest.
//!
//! The guest crate (`extensions/wasm/kiqi-plan-mode`) is a thin shim: it
//! implements [`PlanHost`] over the WIT `session-read` / `session-control` /
//! `inject` imports and forwards events/commands here. Everything that decides
//! behaviour (which tools survive, what `/plan` prints, how a plan is persisted
//! and restored) lives in this crate for one reason: **guest crates link only
//! for `wasm32-wasip2` and cannot carry unit tests**, so the logic would
//! otherwise be untestable. (Before 2026-10-21 a compile-time extension shared this
//! code too; that channel is retired.)
//!
//! The state below is a process-global static by design: this crate is compiled
//! *into* the guest, and each guest instance gets its own copy.

use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

pub const MAX_PLAN_CHARS: usize = 50_000;
pub const PLAN_TOOL: &str = "plan_mode_complete";
pub const START_TOOL: &str = "plan_mode_start";
pub const PLAN_ENTRY: &str = "plan-mode";

/// The five session operations plan mode needs, whichever channel provides them.
pub trait PlanHost {
    fn active_tools(&self) -> Result<Vec<String>, String>;
    fn set_active_tools(&self, tools: &[String]) -> Result<(), String>;
    fn session_branch(&self) -> Result<Value, String>;
    fn append_custom_entry(&self, custom_type: &str, data: &Value) -> Result<(), String>;
    fn send_user_message(&self, text: &str) -> Result<(), String>;
}

static STATE: OnceLock<Mutex<PlanState>> = OnceLock::new();

struct PlanState {
    active: bool,
    plan: Option<String>,
    previous_tools: Option<Vec<String>>,
}

fn state() -> &'static Mutex<PlanState> {
    STATE.get_or_init(|| {
        Mutex::new(PlanState {
            active: false,
            plan: None,
            previous_tools: None,
        })
    })
}

fn current_tools(host: &dyn PlanHost) -> Result<Vec<String>, String> {
    host.active_tools()
}

fn set_tools(host: &dyn PlanHost, tools: &[String]) -> Result<(), String> {
    host.set_active_tools(tools)
}

fn persist_state(host: &dyn PlanHost, active: bool, plan: Option<&str>) {
    let _ = host.append_custom_entry(PLAN_ENTRY, &json!({"active": active, "plan": plan}));
}

fn restore_state(host: &dyn PlanHost) {
    let Ok(value) = host.session_branch() else {
        return;
    };
    let mut restored = None;
    if let Some(entries) = value.get("entries").and_then(Value::as_array) {
        for entry in entries {
            if entry.get("type").and_then(Value::as_str) == Some("custom")
                && entry.get("customType").and_then(Value::as_str) == Some(PLAN_ENTRY)
            {
                restored = entry.get("data").cloned();
            }
        }
    }
    if let Some(data) = restored {
        let active = data.get("active").and_then(Value::as_bool).unwrap_or(false);
        // The persisted entry records the plan, not the tool set it replaced.
        // This process rebuilt its own tool list, so capture the live set now:
        // without it `deactivate` has nothing to put back and would report a
        // restore it never performed. A live set that is already the planning
        // set carries no information (and must not clear a live capture), so
        // it is left as `None`.
        let previous = if active {
            current_tools(host)
                .ok()
                .filter(|tools| tools != &planning_tools())
        } else {
            None
        };
        let mut guard = state().lock().unwrap_or_else(|p| p.into_inner());
        guard.active = active;
        guard.plan = data.get("plan").and_then(Value::as_str).map(str::to_owned);
        if previous.is_some() {
            guard.previous_tools = previous;
        }
    }
}

/// Session start / tree change body, shared by both channels.
pub fn on_session_event(host: &dyn PlanHost) {
    restore_state(host);
}

fn planning_tools() -> Vec<String> {
    vec![
        "read".to_string(),
        "bash".to_string(),
        "docs".to_string(),
        PLAN_TOOL.to_string(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanItem {
    number: usize,
    text: String,
    completed: bool,
}

fn parse_plan_items(plan: &str) -> Vec<PlanItem> {
    let mut items: Vec<PlanItem> = Vec::new();
    let mut done = std::collections::HashSet::new();
    for line in plan.lines() {
        let line = line.trim();
        if let Some(index) = line
            .strip_prefix("[DONE:")
            .and_then(|value| value.strip_suffix(']'))
        {
            if let Ok(index) = index.trim().parse::<usize>() {
                done.insert(index);
            }
        }
    }
    for line in plan.lines() {
        let line = line.trim();
        if line.starts_with("[DONE:") {
            continue;
        }

        let mut candidate = line;
        let checked = if let Some(value) = candidate.strip_prefix("- [x] ") {
            candidate = value;
            true
        } else if let Some(value) = candidate.strip_prefix("- [X] ") {
            candidate = value;
            true
        } else if let Some(value) = candidate.strip_prefix("- [ ] ") {
            candidate = value;
            false
        } else {
            false
        };
        let Some((number, text)) = candidate.split_once('.') else {
            continue;
        };
        let Ok(number) = number.trim().parse::<usize>() else {
            continue;
        };
        let text = text.trim();
        if !text.is_empty() {
            let completed = checked || done.contains(&number);
            if let Some(existing) = items.iter_mut().find(|item| item.number == number) {
                existing.completed |= completed;
            } else {
                items.push(PlanItem {
                    number,
                    text: text.to_string(),
                    completed,
                });
            }
        }
    }
    items
}

fn plan_outline(plan: &str) -> String {
    let items = parse_plan_items(plan);
    if items.is_empty() {
        return "(Plan saved; use `/plan show` to view the full plan.)".to_string();
    }
    let total = items.len();
    let completed = items.iter().filter(|item| item.completed).count();
    let mut lines = vec![format!("Progress: {completed}/{total} complete")];
    for item in items.iter().take(12) {
        let marker = if item.completed { '✓' } else { '○' };
        lines.push(format!("- {marker} {}. {}", item.number, item.text));
    }
    if total > 12 {
        lines.push("... (use `/plan show` to view the remaining steps)".to_string());
    }
    lines.join("\n")
}

fn markdown_plan(plan: Option<&str>, active: bool) -> String {
    let (status, hint) = if active {
        (
            "Planning",
            "Explore first, then call `plan_mode_complete` when the plan is ready.",
        )
    } else if plan.is_some_and(|plan| !plan.trim().is_empty()) {
        (
            "Ready",
            "Use `/plan start` or `plan_mode_start` to create a new plan.",
        )
    } else {
        ("Idle", "Use `/plan start` to begin planning.")
    };
    let body = match plan {
        Some(plan) if !plan.trim().is_empty() => plan.trim().to_string(),
        _ => "_No completed plan yet._".to_string(),
    };
    format!("**Plan · {status}**\n\n{body}\n\n{hint}")
}

fn activate(host: &dyn PlanHost) -> Result<bool, String> {
    let mut guard = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.active {
        return Ok(false);
    }
    let previous = current_tools(host)?;
    set_tools(host, &planning_tools())?;
    guard.previous_tools = Some(previous);
    guard.active = true;
    guard.plan = None;
    drop(guard);
    persist_state(host, true, None);
    Ok(true)
}

fn deactivate(host: &dyn PlanHost) -> Result<Deactivated, String> {
    let mut guard = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard.active {
        return Ok(Deactivated::Inactive);
    }
    // Only clear `previous_tools` once the host accepted the restore: taking
    // it first loses the captured set forever when `set_tools` fails.
    let restored = match guard.previous_tools.clone() {
        Some(previous) => {
            set_tools(host, &previous)?;
            guard.previous_tools = None;
            true
        }
        None => false,
    };
    guard.active = false;
    let plan = guard.plan.clone();
    drop(guard);
    persist_state(host, false, plan.as_deref());
    Ok(if restored {
        Deactivated::Restored
    } else {
        Deactivated::RestoredWithoutTools
    })
}

/// What [`deactivate`] did, so the caller's message can match reality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deactivated {
    /// Plan mode was not active; nothing changed.
    Inactive,
    /// Plan mode ended and the replaced tool set is back.
    Restored,
    /// Plan mode ended, but the replaced tool set was never recorded in this
    /// process (the session was resumed: the stored entry keeps the plan, not
    /// the tools), so the planning set is still active.
    RestoredWithoutTools,
}

fn plan_status() -> String {
    let guard = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    markdown_plan(guard.plan.as_deref(), guard.active)
}

/// Enter plan mode (idempotent) and answer with the tool result the model reads.
pub fn start_plan(host: &dyn PlanHost, raw: &Value) -> Result<String, String> {
    let request = raw
        .get("request")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|request| !request.is_empty());
    activate(host)?;
    let message = request
        .map(|value| format!("Plan mode started for: {value}"))
        .unwrap_or_else(|| {
            "Plan mode started. Inspect the repository, then call `plan_mode_complete`.".to_string()
        });
    Ok(json!({
        "content": [{"type": "text", "text": message}],
        "details": {"kind": "plan", "active": true}
    })
    .to_string())
}

/// Store the finished plan and leave plan mode.
pub fn complete_plan(host: &dyn PlanHost, raw: &Value) -> Result<String, String> {
    let plan = raw
        .get("plan")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .ok_or("plan must be a non-empty string")?;
    if plan.chars().count() > MAX_PLAN_CHARS {
        return Err(format!("plan must not exceed {MAX_PLAN_CHARS} characters"));
    }
    {
        let guard = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !guard.active {
            return Err("plan_mode_complete is only available while plan mode is active".into());
        }
    }
    {
        let mut guard = state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.plan = Some(plan.to_string());
    }
    deactivate(host)?;
    Ok(json!({
        "content": [{"type": "text", "text": markdown_plan(Some(plan), false)}],
        "details": {"kind": "plan", "plan": plan, "outline": plan_outline(plan), "active": false, "markdown": true}
    })
    .to_string())
}

/// The `/plan` command body, shared by both channels.
pub fn run_command(host: &dyn PlanHost, args: &str) -> Value {
    let args = args.trim();
    let first = args.split_whitespace().next().unwrap_or("").to_lowercase();
    let result = match first.as_str() {
        "start" => match activate(host) {
            Ok(true) => {
                "Plan mode enabled. Inspect the codebase and call `plan_mode_complete` when ready."
                    .to_string()
            }
            Ok(false) => "Plan mode is already active.".to_string(),
            Err(error) => format!("Unable to enter plan mode: {error}"),
        },
        "show" | "status" | "" => plan_status(),
        "finalize" => {
            if state()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .plan
                .is_some()
            {
                "Plan is stored in the current session. Use `/plan show` to review it.".to_string()
            } else {
                "No plan yet. Complete plan mode with `plan_mode_complete` first.".to_string()
            }
        }
        "exit" | "off" => match deactivate(host) {
            Ok(Deactivated::Restored) => {
                "Plan mode disabled. Full tool access restored.".to_string()
            }
            Ok(Deactivated::RestoredWithoutTools) => "Plan mode disabled. The tool set it replaced                  was not recorded in this session, so the planning tools are still active; the                  full set comes back on a new session."
                .to_string(),
            Ok(Deactivated::Inactive) => "Plan mode is not active.".to_string(),
            Err(error) => format!("Unable to leave plan mode: {error}"),
        },
        _ => match activate(host) {
            Ok(_) => {
                let prompt = format!(
                    "You are now in Plan mode. Do not edit files or implement changes.\n\nUser request: {args}\n\nInspect the repository, resolve important decisions, and call `plan_mode_complete` with a complete implementation-ready Markdown plan when finished."
                );
                if let Err(error) = host.send_user_message(&prompt) {
                    let _ = deactivate(host);
                    format!("Unable to start planning request: {error}")
                } else {
                    "Plan mode started and the planning request was submitted.".to_string()
                }
            }
            Err(error) => format!("Unable to enter plan mode: {error}"),
        },
    };
    let mut output = json!({"kind": "message", "text": result});
    if !result.starts_with("Unable to") {
        let guard = state().lock().unwrap_or_else(|p| p.into_inner());
        output["details"] = json!({"kind":"plan","plan":guard.plan,"active":guard.active,"expanded":first == "show"});
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that records what plan mode asked it to do.
    #[derive(Default)]
    struct StubHost {
        tools: Mutex<Vec<String>>,
    }

    impl PlanHost for StubHost {
        fn active_tools(&self) -> Result<Vec<String>, String> {
            Ok(vec!["read".into(), "bash".into(), "edit".into()])
        }
        fn set_active_tools(&self, tools: &[String]) -> Result<(), String> {
            *self.tools.lock().unwrap() = tools.to_vec();
            Ok(())
        }
        fn session_branch(&self) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn append_custom_entry(&self, _custom_type: &str, _data: &Value) -> Result<(), String> {
            Ok(())
        }
        fn send_user_message(&self, _text: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn renders_empty_status() {
        assert!(markdown_plan(None, false).contains("No completed plan"));
        assert!(markdown_plan(None, false).contains("Idle"));
        assert!(!markdown_plan(None, false).contains("Ready"));
    }

    #[test]
    fn rejects_empty_plan() {
        let host = StubHost::default();
        assert!(complete_plan(&host, &json!({"plan": ""})).is_err());
    }

    #[test]
    fn extracts_numbered_outline_without_markdown() {
        let outline = plan_outline("## Plan\n\n1. Add the parser\n2. Update the UI\n\nDetails");
        assert_eq!(
            outline,
            "Progress: 0/2 complete\n- ○ 1. Add the parser\n- ○ 2. Update the UI"
        );
    }

    #[test]
    fn marks_done_steps_from_tags_and_checkboxes() {
        let items = parse_plan_items("1. First\n2. Second\n[DONE:1]\n- [x] 2. Second");
        assert!(items[0].completed);
        assert!(items[1].completed);
        assert!(plan_outline("1. First\n2. Second\n[DONE:1]").contains("Progress: 1/2 complete"));
    }

    #[test]
    fn limits_long_outline() {
        let plan = (1..=13)
            .map(|index| format!("{index}. Step {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outline = plan_outline(&plan);
        assert!(outline.contains("... (use `/plan show` to view the remaining steps)"));
        assert_eq!(outline.lines().count(), 14);
    }

    #[test]
    fn complete_display_preserves_sections_and_non_numbered_plans() {
        let plan =
            "## Approach\n\n- Inspect source\n- Update UI\n\n## Tests\n\nRun the regression suite";
        let rendered = markdown_plan(Some(plan), false);
        assert!(rendered.contains(plan));
        assert_eq!(rendered.matches("Ready").count(), 1);
        assert!(!rendered.contains("Next:"));
    }
}
