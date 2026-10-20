#![forbid(unsafe_code)]

pub mod kiqi;

use kiqi_plugin_sdk::handles::ToolHandles;
use kiqi_plugin_sdk::{
    CommandHandlerFn, EventHandlerFn, EventTag, FreeStringFn, PluginApi, RuntimeActionId,
    StablePluginEvent, StableToolSchema, StbString, StbStringRef, StepHandle, StepResult,
    ToolPartialCb,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const GOAL_FILE: &str = ".ki/goals.json";
/// The custom-message type stored in the generic AgentMessage payload.
const GOAL_CUSTOM_TYPE: &str = "goal";
const MAX_TITLE_CHARS: usize = 500;
const MAX_NOTES_CHARS: usize = 20_000;

// Reuse the SDK's host-context lifetime and thread-safety contract.
type Runtime = PluginApi;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
/// Last injected goal-state fingerprint (title + status + notes). Prevents
/// re-injecting the same status on every `TurnEnd` (transcript bloat).
static LAST_INJECTED: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn last_injected() -> &'static Mutex<Option<String>> {
    LAST_INJECTED.get_or_init(|| Mutex::new(None))
}

/// Call a host runtime action (e.g. `AppendEntry`). Safe from any thread,
/// including event handlers and command handlers.
fn call_runtime(id: RuntimeActionId, args: Value) -> Result<Value, String> {
    let runtime = *RUNTIME.get().ok_or("goal runtime is unavailable")?;
    let input = args.to_string();
    let mut output = StbString::empty();
    let rc = (runtime.runtime_action)(
        id as u32,
        StbStringRef::from_str(&input),
        &mut output,
        runtime.user_data,
    );
    let text = output.to_string_lossy();
    (runtime.free_string)(output);
    if rc != 0 {
        return Err(if text.is_empty() {
            format!("runtime action {id:?} failed")
        } else {
            text
        });
    }
    if text.trim().is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_str(&text).map_err(|e| format!("invalid runtime response: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Goal {
    title: String,
    status: String, // active | paused | complete
    notes: String,
    started_at: i64,
    updated_at: i64,
    /// Turns completed while the goal was active. Drives the `max_turns` budget.
    #[serde(default)]
    turns_used: u32,
    /// Turn budget. `None` = unlimited.
    #[serde(default)]
    max_turns: Option<u32>,
    /// Absolute epoch-ms deadline. `None` = unlimited.
    #[serde(default)]
    deadline_ms: Option<i64>,
    /// Explicit definition of done. The host cannot evaluate arbitrary
    /// conditions, so this is injected into the model context as the stopping
    /// criterion the agent must check before calling `complete`; it also makes
    /// the completion auditable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    done_when: Option<String>,
}

impl Goal {
    /// Human-readable budget line, e.g. `turns 3/20 · time 12m left`.
    fn budget_summary(&self, now: i64) -> String {
        let mut parts = Vec::new();
        match self.max_turns {
            Some(max) => parts.push(format!("turns {}/{}", self.turns_used.min(max), max)),
            None => parts.push(format!("turns {} (unlimited)", self.turns_used)),
        }
        match self.deadline_ms {
            Some(deadline) => {
                let remaining = deadline - now;
                if remaining <= 0 {
                    parts.push("time is up".to_string());
                } else {
                    parts.push(format!("{} left", format_duration_ms(remaining)));
                }
            }
            None => parts.push("time unlimited".to_string()),
        }
        parts.join(" · ")
    }

    /// Static budget hint injected into the model context (no live counters, so
    /// the message stays fingerprint-stable across turns).
    fn budget_hint(&self) -> String {
        match (self.max_turns, self.deadline_ms) {
            (None, None) => String::new(),
            (max, deadline) => {
                let mut parts = Vec::new();
                if let Some(max) = max {
                    parts.push(format!("{max} turns"));
                }
                if let Some(deadline) = deadline {
                    let remaining = deadline - self.started_at;
                    parts.push(format_duration_ms(remaining.max(0)));
                }
                format!(" (budget: {})", parts.join(" / "))
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Goals {
    active: Option<Goal>,
    archived: Vec<Goal>,
}

fn goals_path() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(GOAL_FILE)
}

pub(crate) fn read_goals() -> Goals {
    let path = goals_path();
    let Ok(text) = fs::read_to_string(&path) else {
        return Goals::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|_| Goals::default())
}

pub(crate) fn write_goals(goals: &Goals) -> Result<(), String> {
    let path = goals_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create goal directory: {e}"))?;
    }
    let tmp = path.with_file_name(format!("goals.json.tmp-{}", std::process::id()));
    let data = serde_json::to_string_pretty(goals).map_err(|e| format!("serialize goals: {e}"))?;
    fs::write(&tmp, data).map_err(|e| format!("write goals: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("commit goals: {e}"))?;
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Goal-state fingerprint used to dedupe injections. Deliberately excludes the
/// live counters (`turns_used`) so a running budget does not re-inject each turn.
pub(crate) fn fingerprint(goal: &Goal) -> String {
    format!(
        "{}|{}|{}|{}",
        goal.title,
        goal.status,
        goal.notes,
        goal.done_when.as_deref().unwrap_or("")
    )
}

/// Mark the current active-goal status as already injected.
pub(crate) fn remember_injected(fingerprint: Option<String>) {
    let mut guard = last_injected().lock().unwrap_or_else(|p| p.into_inner());
    *guard = fingerprint;
}

/// Should we inject now? False when the same goal-state was already appended.
pub(crate) fn should_inject(fingerprint: Option<&str>) -> bool {
    let guard = last_injected().lock().unwrap_or_else(|p| p.into_inner());
    guard.as_deref() != fingerprint
}

// ---------------------------------------------------------------------------
// Budget parsing / formatting
// ---------------------------------------------------------------------------

/// Parse a duration spec into milliseconds. Accepts bare numbers (minutes) and
/// unit suffixes `s`/`m`/`h`/`d`, combinable (`90s`, `30m`, `2h`, `1h30m`).
fn parse_duration_ms(spec: &str) -> Result<i64, String> {
    let spec = spec.trim().to_lowercase();
    if spec.is_empty() {
        return Err("empty duration".into());
    }
    if let Ok(minutes) = spec.parse::<i64>() {
        return Ok(minutes.saturating_mul(60_000));
    }
    let mut total: i64 = 0;
    let mut number = String::new();
    let mut saw_unit = false;
    for ch in spec.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        let unit = match ch {
            's' => 1_000,
            'm' => 60_000,
            'h' => 3_600_000,
            'd' => 86_400_000,
            other => return Err(format!("unknown duration unit `{other}` (use s/m/h/d)")),
        };
        let value: i64 = number
            .parse()
            .map_err(|_| format!("invalid duration `{spec}`"))?;
        total = total.saturating_add(value.saturating_mul(unit));
        number.clear();
        saw_unit = true;
    }
    if !number.is_empty() {
        return Err(format!("duration `{spec}` is missing a unit (s/m/h/d)"));
    }
    if !saw_unit {
        return Err(format!("invalid duration `{spec}`"));
    }
    Ok(total)
}

/// Compact duration rendering: `2h5m`, `12m30s`, `45s`.
fn format_duration_ms(ms: i64) -> String {
    let secs = (ms.max(0) / 1000) as u64;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

// ---------------------------------------------------------------------------
// Context injection
// ---------------------------------------------------------------------------

/// The one host operation goal needs. Two adapters keep both plugin channels
/// working without the core reaching for a global: the cdylib face goes through
/// `call_runtime`, the compile-time face through `HostCaps`.
pub(crate) trait GoalHost {
    fn append_message(&self, message: &Value) -> Result<(), String>;
}

/// cdylib adapter: the existing C-ABI path, unchanged.
struct AbiHost;

impl GoalHost for AbiHost {
    fn append_message(&self, message: &Value) -> Result<(), String> {
        call_runtime(RuntimeActionId::AppendEntry, json!({"message": message}))?;
        Ok(())
    }
}

/// compile-time adapter: the caller already holds the capabilities.
pub(crate) struct CapsHost<'a>(pub &'a kiqi_plugin::HostCaps<'a>);

impl GoalHost for CapsHost<'_> {
    fn append_message(&self, message: &Value) -> Result<(), String> {
        let inject = self
            .0
            .inject
            .ok_or("the host did not grant message injection to this goal plugin")?;
        inject
            .append_message(message)
            .map_err(|error| error.to_string())
    }
}

fn append_custom_message(host: &dyn GoalHost, goal: &Goal, text: String) -> Result<(), String> {
    let message = json!({
        "kind": "custom",
        "role": "custom",
        "content": [{"type": "text", "text": text}],
        "data": {
            "customType": GOAL_CUSTOM_TYPE,
            "display": true,
            "details": {
                "title": goal.title,
                "status": goal.status,
                "notes": goal.notes,
                "turnsUsed": goal.turns_used,
                "maxTurns": goal.max_turns,
                "deadlineMs": goal.deadline_ms,
                "startedAt": goal.started_at,
                "updatedAt": goal.updated_at,
            }
        },
        "timestamp": goal.updated_at,
    });
    host.append_message(&message)
}

/// Append the goal's current state as a generic custom message. The host stores
/// it as an ordinary message entry and the generic context builder forwards it
/// to the model; no goal-specific host projector is required.
fn append_goal_custom_message(host: &dyn GoalHost, goal: &Goal) -> Result<(), String> {
    let tag = match goal.status.as_str() {
        "paused" => "⏸️ Paused goal",
        "complete" => "✅ Goal completed",
        _ => "🎯 Active goal",
    };
    let notes = goal.notes.trim();
    let hint = goal.budget_hint();
    let mut text = if notes.is_empty() {
        format!("{tag}: {}.{hint}", goal.title)
    } else {
        format!("{tag}: {} — {}.{hint}", goal.title, notes)
    };
    if let Some(done_when) = goal.done_when.as_deref() {
        text.push_str(&format!("\nDefinition of done: {done_when}"));
    }
    append_custom_message(host, goal, text)
}

/// Announce that a budget was exhausted and the goal auto-paused.
fn append_budget_message(host: &dyn GoalHost, goal: &Goal, reason: &str) -> Result<(), String> {
    let text = format!(
        "⏸️ Goal paused — {reason}: {}. Resume with `goal resume` or `/goal resume`.",
        goal.title
    );
    append_custom_message(host, goal, text)
}

// ---------------------------------------------------------------------------
// Core operations (shared by the `goal` tool and the `/goal` command)
// ---------------------------------------------------------------------------

fn require_active(goals: &Goals) -> Result<Goal, String> {
    goals
        .active
        .clone()
        .ok_or_else(|| "no active goal — start one first".to_string())
}

fn validate_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("goal title is required".into());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!(
            "title must not exceed {MAX_TITLE_CHARS} characters"
        ));
    }
    Ok(title.to_string())
}

fn validate_done_when(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.chars().count() > MAX_NOTES_CHARS {
        return Err(format!(
            "definition of done must not exceed {MAX_NOTES_CHARS} characters"
        ));
    }
    Ok(value.to_string())
}

fn validate_notes(notes: &str) -> Result<String, String> {
    let notes = notes.trim();
    if notes.chars().count() > MAX_NOTES_CHARS {
        return Err(format!(
            "notes must not exceed {MAX_NOTES_CHARS} characters"
        ));
    }
    Ok(notes.to_string())
}

/// Persist `goal` as the active goal and inject it into the model context.
fn persist_and_inject(host: &dyn GoalHost, goals: &Goals, goal: &Goal) -> Result<(), String> {
    let mut goals = goals.clone();
    goals.active = Some(goal.clone());
    write_goals(&goals)?;
    let _ = append_goal_custom_message(host, goal);
    remember_injected(Some(fingerprint(goal)));
    Ok(())
}

fn op_start(
    host: &dyn GoalHost,
    title: &str,
    notes: &str,
    max_turns: Option<u32>,
    deadline_ms: Option<i64>,
    done_when: Option<&str>,
) -> Result<Goal, String> {
    let title = validate_title(title)?;
    let notes = validate_notes(notes)?;
    let done_when = done_when
        .map(validate_done_when)
        .transpose()?
        .filter(|value| !value.is_empty());
    let now = now_ms();
    let goal = Goal {
        title,
        status: "active".into(),
        notes,
        started_at: now,
        updated_at: now,
        turns_used: 0,
        max_turns,
        deadline_ms,
        done_when,
    };
    let mut goals = read_goals();
    if let Some(old) = goals.active.take() {
        goals.archived.push(old);
    }
    persist_and_inject(host, &goals, &goal)?;
    Ok(goal)
}

fn op_update(
    host: &dyn GoalHost,
    notes: Option<&str>,
    status: Option<&str>,
) -> Result<Goal, String> {
    let goals = read_goals();
    let mut updated = require_active(&goals)?;
    if let Some(notes) = notes {
        updated.notes = validate_notes(notes)?;
    }
    if let Some(status) = status {
        if !["active", "paused"].contains(&status) {
            return Err("status must be \"active\" or \"paused\"".into());
        }
        updated.status = status.to_string();
    }
    updated.updated_at = now_ms();
    persist_and_inject(host, &goals, &updated)?;
    Ok(updated)
}

fn op_set_status(host: &dyn GoalHost, status: &str) -> Result<Goal, String> {
    op_update(host, None, Some(status))
}

/// Merge budget changes into `goal`. `None` leaves a field untouched;
/// `Some(None)` clears it; `Some(Some(v))` sets it (a relative ms value for
/// the deadline, resolved against `now`).
fn apply_limits(
    goal: &Goal,
    now: i64,
    max_turns: Option<Option<u32>>,
    deadline_ms: Option<Option<i64>>,
) -> Goal {
    let mut updated = goal.clone();
    if let Some(max_turns) = max_turns {
        updated.max_turns = max_turns.filter(|n| *n > 0);
    }
    if let Some(deadline_ms) = deadline_ms {
        updated.deadline_ms = match deadline_ms {
            Some(ms) if ms > 0 => Some(now.saturating_add(ms)),
            _ => None,
        };
    }
    updated.updated_at = now;
    updated
}

fn op_set_limits(
    host: &dyn GoalHost,
    max_turns: Option<Option<u32>>,
    deadline_ms: Option<Option<i64>>,
) -> Result<Goal, String> {
    let goals = read_goals();
    let active = require_active(&goals)?;
    let updated = apply_limits(&active, now_ms(), max_turns, deadline_ms);
    persist_and_inject(host, &goals, &updated)?;
    Ok(updated)
}

fn op_complete(host: &dyn GoalHost) -> Result<Goal, String> {
    let mut goals = read_goals();
    let mut done = require_active(&goals)?;
    done.status = "complete".into();
    done.updated_at = now_ms();
    goals.archived.push(done.clone());
    goals.active = None;
    write_goals(&goals)?;
    let _ = append_goal_custom_message(host, &done);
    remember_injected(Some(fingerprint(&done)));
    Ok(done)
}

// ---------------------------------------------------------------------------
// Rendering (tool = markdown, command = plain text)
// ---------------------------------------------------------------------------

fn render_status_text(goal: Option<&Goal>) -> String {
    let now = now_ms();
    match goal {
        Some(goal) => {
            let notes = if goal.notes.trim().is_empty() {
                "(no notes)".to_string()
            } else {
                goal.notes.clone()
            };
            let done = goal
                .done_when
                .as_deref()
                .map(|value| format!("\ndefinition of done: {value}"))
                .unwrap_or_default();
            format!(
                "🎯 {}\nstatus: {}\nnotes: {}\n{}{}",
                goal.title,
                goal.status,
                notes,
                goal.budget_summary(now),
                done
            )
        }
        None => "🎯 No active goal. Start one with `/goal start <title>`.".to_string(),
    }
}

fn render_list_text(goals: &Goals) -> String {
    let now = now_ms();
    let mut lines = vec!["🎯 Goals".to_string()];
    match &goals.active {
        Some(goal) => lines.push(format!(
            "• {} ({}) — {}",
            goal.title,
            goal.status,
            goal.budget_summary(now)
        )),
        None => lines.push("• (no active goal)".to_string()),
    }
    if !goals.archived.is_empty() {
        lines.push(format!("archived: {}", goals.archived.len()));
        for goal in &goals.archived {
            lines.push(format!("  – {}", goal.title));
        }
    }
    lines.join("\n")
}

fn goal_summary_json(goal: &Goal) -> Value {
    json!({
        "title": goal.title,
        "status": goal.status,
        "notes": goal.notes,
        "turnsUsed": goal.turns_used,
        "maxTurns": goal.max_turns,
        "deadlineMs": goal.deadline_ms,
        "doneWhen": goal.done_when,
        "startedAt": goal.started_at,
        "updatedAt": goal.updated_at,
    })
}

fn tool_envelope(action: &str, text: String, active: bool, goal: Option<&Goal>) -> String {
    json!({
        "content": [{"type": "text", "text": text}],
        "details": {
            "kind": "goal",
            "action": action,
            "active": active,
            "markdown": true,
            "goal": goal.map(goal_summary_json),
        }
    })
    .to_string()
}

pub(crate) fn show_goal() -> Result<String, String> {
    let goals = read_goals();
    let text = match &goals.active {
        Some(goal) => format!(
            "## 🎯 Current goal\n\n**{}**  ({})\n\n{}\n\n{}",
            goal.title,
            goal.status,
            if goal.notes.trim().is_empty() {
                "_no notes_".to_string()
            } else {
                goal.notes.clone()
            },
            goal.budget_summary(now_ms())
        ),
        None => "## 🎯 Goal\n\n_No active goal._ Start one with `goal start`.".to_string(),
    };
    Ok(tool_envelope(
        "show",
        text,
        goals.active.is_some(),
        goals.active.as_ref(),
    ))
}

pub(crate) fn list_goals() -> Result<String, String> {
    let goals = read_goals();
    let mut lines = vec!["## 🎯 Goals".to_string()];
    match &goals.active {
        Some(goal) => lines.push(format!(
            "- **{}** ({}): {}",
            goal.title,
            goal.status,
            if goal.notes.trim().is_empty() {
                "_no notes_".to_string()
            } else {
                goal.notes.clone()
            }
        )),
        None => lines.push("_No active goal._".to_string()),
    }
    if !goals.archived.is_empty() {
        lines.push("".to_string());
        lines.push("### Archived".to_string());
        for goal in &goals.archived {
            lines.push(format!("- ~~{}~~", goal.title));
        }
    }
    Ok(tool_envelope(
        "list",
        lines.join("\n"),
        goals.active.is_some(),
        goals.active.as_ref(),
    ))
}

// ---------------------------------------------------------------------------
// Tool actions
// ---------------------------------------------------------------------------

fn param_u32(params: &Value, key: &str) -> Option<u32> {
    // Clamp rather than truncate: `as u32` keeps only the low 32 bits, so
    // maxTurns 4294967297 silently became 1 (and 4294967296 became 0, which
    // drops the budget entirely).
    params
        .get(key)
        .and_then(Value::as_u64)
        .map(|v| v.min(u32::MAX as u64) as u32)
}

pub(crate) fn start_goal(host: &dyn GoalHost, params: &Value) -> Result<String, String> {
    let title = params
        .get("title")
        .and_then(Value::as_str)
        .ok_or("goal title is required")?;
    let notes = params.get("notes").and_then(Value::as_str).unwrap_or("");
    let max_turns = param_u32(params, "maxTurns").filter(|n| *n > 0);
    let deadline_ms = param_u32(params, "maxMinutes")
        .filter(|n| *n > 0)
        .map(|minutes| now_ms().saturating_add((minutes as i64) * 60_000));
    let done_when = params.get("doneWhen").and_then(Value::as_str);
    let goal = op_start(host, title, notes, max_turns, deadline_ms, done_when)?;
    let text = format!(
        "## 🎯 Goal started\n\n**{}**\n\n{}\n\n_{}_",
        goal.title,
        if goal.notes.trim().is_empty() {
            "_no notes_".to_string()
        } else {
            goal.notes.clone()
        },
        goal.budget_summary(now_ms())
    );
    Ok(tool_envelope("start", text, true, Some(&goal)))
}

pub(crate) fn update_goal(host: &dyn GoalHost, params: &Value) -> Result<String, String> {
    let notes = params.get("notes").and_then(Value::as_str);
    let status = params.get("status").and_then(Value::as_str);
    if notes.is_none() && status.is_none() {
        return Err("update requires `notes` and/or `status`".into());
    }
    let goal = op_update(host, notes, status)?;
    let text = format!(
        "## 🎯 Goal updated\n\n**{}**  ({})\n\n{}\n\n{}",
        goal.title,
        goal.status,
        if goal.notes.trim().is_empty() {
            "_no notes_".to_string()
        } else {
            goal.notes.clone()
        },
        goal.budget_summary(now_ms())
    );
    Ok(tool_envelope("update", text, true, Some(&goal)))
}

pub(crate) fn status_goal(host: &dyn GoalHost, status: &str) -> Result<String, String> {
    let goal = op_set_status(host, status)?;
    let verb = if status == "paused" {
        "paused"
    } else {
        "resumed"
    };
    let text = format!("## 🎯 Goal {verb}\n\n**{}**", goal.title);
    Ok(tool_envelope(
        if status == "paused" {
            "pause"
        } else {
            "resume"
        },
        text,
        true,
        Some(&goal),
    ))
}

pub(crate) fn complete_goal(host: &dyn GoalHost) -> Result<String, String> {
    let goal = op_complete(host)?;
    let text = format!(
        "## ✅ Goal complete\n\n**{}**  \n\nArchived. The goal is no longer injected into context.",
        goal.title
    );
    Ok(tool_envelope("complete", text, false, Some(&goal)))
}

pub(crate) fn limit_goal(host: &dyn GoalHost, params: &Value) -> Result<String, String> {
    let max_turns = params
        .get("maxTurns")
        .and_then(Value::as_u64)
        .map(|v| (v.min(u32::MAX as u64) as u32).max(1));
    // Saturating: a raw `* 60_000` overflowed i64 for large values (debug
    // panic, release wrap), and a wrapped-negative deadline was read as
    // "clear the budget" while the tool reported an update.
    let max_minutes = params
        .get("maxMinutes")
        .and_then(Value::as_u64)
        .map(|v| (v.min(i64::MAX as u64) as i64).saturating_mul(60_000));
    if max_turns.is_none() && max_minutes.is_none() {
        return Err("limit requires `maxTurns` and/or `maxMinutes`".into());
    }
    // `Some(None)` clears a budget, `None` leaves it untouched.
    let goal = op_set_limits(host, max_turns.map(Some), max_minutes.map(Some))?;
    let text = format!(
        "## 🎯 Goal budget updated\n\n**{}**\n\n{}",
        goal.title,
        goal.budget_summary(now_ms())
    );
    Ok(tool_envelope("limit", text, true, Some(&goal)))
}

pub(crate) fn goal(host: &dyn GoalHost, params: &Value) -> Result<String, String> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("show")
        .to_lowercase();
    match action.as_str() {
        "start" => start_goal(host, params),
        "update" => update_goal(host, params),
        "pause" => status_goal(host, "paused"),
        "resume" => status_goal(host, "active"),
        "complete" => complete_goal(host),
        "limit" => limit_goal(host, params),
        "show" | "status" => show_goal(),
        "list" => list_goals(),
        _ => Err(format!("unknown goal action: {action}")),
    }
}

// ---------------------------------------------------------------------------
// Tool drive (ABI)
// ---------------------------------------------------------------------------

static HANDLES: ToolHandles<Drive> = ToolHandles::new();

struct Drive {
    params: Value,
    done: bool,
}

extern "C" fn execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    let text = params.to_string_lossy();
    params.free_with(free);
    HANDLES.insert(Drive {
        params: serde_json::from_str(&text).unwrap_or(Value::Null),
        done: false,
    })
}

extern "C" fn poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    if handle.is_null() {
        return StepResult::err(StbString::from_string("null goal handle".into()));
    }
    let Some(entry) = HANDLES.get(handle) else {
        return StepResult::err(StbString::from_string(
            "invalid or destroyed tool handle".into(),
        ));
    };
    let mut drive = entry.lock();
    if entry.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
        drive.done = true;
        return StepResult::err(StbString::from_string("goal cancelled".into()));
    }
    if drive.done {
        return StepResult::err(StbString::from_string(
            "goal polled after completion".into(),
        ));
    }
    drive.done = true;
    match goal(&AbiHost, &drive.params) {
        Ok(value) => StepResult::done(StbString::from_string(value)),
        Err(error) => StepResult::err(StbString::from_string(error)),
    }
}

extern "C" fn cancel(handle: StepHandle) {
    HANDLES.cancel(handle);
}

extern "C" fn destroy(handle: StepHandle) {
    drop(HANDLES.remove(handle));
}

use kiqi_plugin_sdk::callbacks::free_string;

// ---------------------------------------------------------------------------
// TurnEnd handler — count turns, enforce budgets, inject the active goal
// ---------------------------------------------------------------------------

extern "C" fn on_turn_end(_event: StablePluginEvent, _: *mut c_void) -> i32 {
    turn_end(&AbiHost)
}

/// The TurnEnd body, shared by both channels. TurnEnd carries no payload.
pub(crate) fn turn_end(host: &dyn GoalHost) -> i32 {
    let mut goals = read_goals();
    let Some(mut goal) = goals.active.clone() else {
        // No active goal — nothing to inject.
        remember_injected(None);
        return 0;
    };
    if goal.status != "active" {
        remember_injected(Some(fingerprint(&goal)));
        return 0;
    }

    // Count this turn against the budget.
    goal.turns_used = goal.turns_used.saturating_add(1);
    goal.updated_at = now_ms();

    let exhausted = match goal.max_turns {
        Some(max) if goal.turns_used >= max => Some(format!("turn budget reached ({max})")),
        _ => match goal.deadline_ms {
            Some(deadline) if goal.updated_at >= deadline => Some("time budget reached".into()),
            _ => None,
        },
    };

    if let Some(reason) = exhausted {
        goal.status = "paused".into();
        goals.active = Some(goal.clone());
        let _ = write_goals(&goals);
        let _ = append_budget_message(host, &goal, &reason);
        remember_injected(Some(fingerprint(&goal)));
        return 0;
    }

    goals.active = Some(goal.clone());
    let _ = write_goals(&goals);

    let fp = fingerprint(&goal);
    if !should_inject(Some(&fp)) {
        return 0;
    }
    match append_goal_custom_message(host, &goal) {
        Ok(_) => {
            remember_injected(Some(fp));
        }
        Err(_) => {
            // Non-fatal: keep last-injected unset so a later TurnEnd retries.
        }
    }
    0
}

// ---------------------------------------------------------------------------
// `/goal` slash command
// ---------------------------------------------------------------------------

fn command_output(out: &mut kiqi_plugin_sdk::callbacks::Output<'_>, value: Value) -> i32 {
    if !out.is_available() {
        return 1;
    }
    out.write(value.to_string());
    0
}

/// Extract `title` / `--notes` from a free-form string.
pub(crate) fn split_title_notes(rest: &str) -> (String, Option<String>) {
    let mut title_parts: Vec<String> = Vec::new();
    let mut notes: Option<String> = None;
    let mut tokens = rest.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        if token == "--notes" || token == "-n" {
            let mut collected = Vec::new();
            while let Some(next) = tokens.peek() {
                if next.starts_with("--") {
                    break;
                }
                collected.push(tokens.next().unwrap().to_string());
            }
            notes = Some(collected.join(" "));
            continue;
        }
        title_parts.push(token.to_string());
    }
    (title_parts.join(" "), notes)
}

const COMMAND_HELP: &str = "\
/goal                     show status
/goal start <title>       start a goal (--notes \"...\" optional)
/goal <title...>          shorthand for start
/goal pause | resume      pause / resume the active goal
/goal stop                complete and archive the goal
/goal turns <N>           max turns (0 clears)
/goal time <dur>          max duration, e.g. 30m / 2h / 90s / 1h30m (0 clears)
/goal limit <N> [<dur>]   set both budgets
/goal clear               clear both budgets
/goal list                list active + archived goals
/goal help                this help";

pub(crate) fn run_command(host: &dyn GoalHost, args: &str) -> String {
    let args = args.trim();
    let (head, rest) = match args.split_once(char::is_whitespace) {
        Some((head, rest)) => (head.to_lowercase(), rest.trim()),
        None => (args.to_lowercase(), ""),
    };

    let result: Result<String, String> = match head.as_str() {
        "" | "status" | "show" => {
            let goals = read_goals();
            Ok(render_status_text(goals.active.as_ref()))
        }
        "list" => Ok(render_list_text(&read_goals())),
        "help" => Ok(COMMAND_HELP.to_string()),
        "start" => {
            let (title, notes) = split_title_notes(rest);
            op_start(
                host,
                &title,
                notes.as_deref().unwrap_or(""),
                None,
                None,
                None,
            )
            .map(|goal| {
                format!(
                    "🎯 Goal started: {}\n{}",
                    goal.title,
                    goal.budget_summary(now_ms())
                )
            })
        }
        "pause" => op_set_status(host, "paused").map(|goal| format!("⏸️ Paused: {}", goal.title)),
        "resume" => op_set_status(host, "active").map(|goal| format!("▶️ Resumed: {}", goal.title)),
        "stop" | "done" | "complete" => {
            op_complete(host).map(|goal| format!("✅ Completed: {}", goal.title))
        }
        "turns" => match rest.parse::<u32>() {
            Ok(0) => op_set_limits(host, Some(None), None)
                .map(|goal| format!("Budget updated: {}", goal.budget_summary(now_ms()))),
            Ok(n) => op_set_limits(host, Some(Some(n)), None)
                .map(|goal| format!("Budget updated: {}", goal.budget_summary(now_ms()))),
            Err(_) => Err(format!("`{rest}` is not a turn count")),
        },
        "time" => match parse_duration_ms(rest) {
            Ok(0) => op_set_limits(host, None, Some(None))
                .map(|goal| format!("Budget updated: {}", goal.budget_summary(now_ms()))),
            Ok(ms) => op_set_limits(host, None, Some(Some(ms)))
                .map(|goal| format!("Budget updated: {}", goal.budget_summary(now_ms()))),
            Err(error) => Err(error),
        },
        "limit" => {
            let mut parts = rest.split_whitespace();
            let turns = parts.next();
            let time = parts.next();
            if turns.is_none() && time.is_none() {
                return "Usage: /goal limit <turns> [duration]".to_string();
            }
            let max_turns = match turns {
                Some(token) => match token.parse::<u32>() {
                    Ok(0) => Some(None),
                    Ok(n) => Some(Some(n)),
                    Err(_) => return format!("`{token}` is not a turn count"),
                },
                None => None,
            };
            let deadline = match time {
                Some(token) => match parse_duration_ms(token) {
                    Ok(0) => Some(None),
                    Ok(ms) => Some(Some(ms)),
                    Err(error) => return error,
                },
                None => None,
            };
            op_set_limits(host, max_turns, deadline)
                .map(|goal| format!("Budget updated: {}", goal.budget_summary(now_ms())))
        }
        "clear" => op_set_limits(host, Some(None), Some(None))
            .map(|goal| format!("Budget cleared: {}", goal.budget_summary(now_ms()))),
        // Unknown verb → treat the whole input as a title (shorthand start).
        _ => {
            let (title, notes) = split_title_notes(args);
            op_start(
                host,
                &title,
                notes.as_deref().unwrap_or(""),
                None,
                None,
                None,
            )
            .map(|goal| format!("🎯 Goal started: {}", goal.title))
        }
    };

    match result {
        Ok(text) => text,
        Err(error) => format!("⚠️ {error}"),
    }
}

kiqi_plugin_sdk::string_handler! {
fn goal_command(args_json, out, _user_data) {
    let raw = args_json.to_owned();
    let envelope: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let args = envelope
        .get("args")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let text = run_command(&AbiHost, args);
    command_output(out, json!({"kind": "message", "text": text}))
}
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------
kiqi_plugin_sdk::export_plugin!(|api| {
    let _ = RUNTIME.set(*api);
    if let Some(register_event) = api.register_event_handler {
        let rc = register_event(
            EventTag::TurnEnd,
            on_turn_end as EventHandlerFn,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return rc;
        }
    }
    if let Some(register_command) = api.register_command {
        let name = StbStringRef::from_str("goal");
        let description =
            StbStringRef::from_str("Show, start, pause, stop, or budget the active goal");
        let _ = register_command(name, description, goal_command as CommandHandlerFn);
    }
    let Some(register) = api.register_tool else {
        return 1;
    };
    let schema = Box::new(StableToolSchema {
        name: StbString::from_string("goal".into()),
        description: StbString::from_string(
            "Codex-style goal mode: record one persistent, context-injected objective per project and let the agent advance it across turns. Actions: start, update, pause, resume, complete, limit, show, list.".into(),
        ),
        parameters: StbString::from_string(
            r#"{"type":"object","properties":{"action":{"type":"string","enum":["start","update","pause","resume","complete","limit","show","list"]},"title":{"type":"string","description":"Goal title (required for start)"},"notes":{"type":"string","description":"Goal notes or progress update (start/update)"},"status":{"type":"string","enum":["active","paused"],"description":"Explicit status change (update)"},"maxTurns":{"type":"integer","minimum":1,"description":"Optional turn budget (start/limit)"},"maxMinutes":{"type":"integer","minimum":1,"description":"Optional wall-clock budget in minutes (start/limit)"},"doneWhen":{"type":"string","description":"Definition of done: the condition that must hold before the agent marks the goal complete"}},"required":["action"]}"#.into(),
        ),
    });
    let rc = register(&*schema, execute, poll, cancel, destroy, free_string);
    drop(schema);
    rc
});

#[cfg(test)]
mod tests {
    use super::*;

    fn goal_with(status: &str) -> Goal {
        Goal {
            title: "t".into(),
            status: status.into(),
            notes: "n".into(),
            started_at: 1,
            updated_at: 1,
            turns_used: 0,
            max_turns: None,
            deadline_ms: None,
            done_when: None,
        }
    }

    #[test]
    fn rejects_empty_title() {
        assert!(validate_title("  ").is_err());
        assert!(validate_title("ok").is_ok());
    }

    #[test]
    fn rejects_unknown_action() {
        assert!(goal(&AbiHost, &json!({"action": "bogus"})).is_err());
    }

    #[test]
    fn fingerprint_changes_with_state() {
        let a = goal_with("active");
        let mut b = a.clone();
        b.notes = "n2".into();
        assert_ne!(fingerprint(&a), fingerprint(&b));
        let mut c = a.clone();
        c.status = "paused".into();
        assert_ne!(fingerprint(&a), fingerprint(&c));
        assert_eq!(fingerprint(&a), fingerprint(&a));
    }

    #[test]
    fn fingerprint_ignores_turn_counter() {
        let a = goal_with("active");
        let mut b = a.clone();
        b.turns_used = 7;
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration_ms("30m").unwrap(), 1_800_000);
        assert_eq!(parse_duration_ms("2h").unwrap(), 7_200_000);
        assert_eq!(parse_duration_ms("90s").unwrap(), 90_000);
        assert_eq!(parse_duration_ms("1h30m").unwrap(), 5_400_000);
        assert_eq!(parse_duration_ms("5").unwrap(), 300_000); // bare = minutes
        assert_eq!(parse_duration_ms("0").unwrap(), 0);
        assert!(parse_duration_ms("10x").is_err());
        assert!(parse_duration_ms("").is_err());
        assert!(parse_duration_ms("h").is_err());
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration_ms(90_000), "1m30s");
        assert_eq!(format_duration_ms(45_000), "45s");
        assert_eq!(format_duration_ms(7_500_000), "2h5m");
        assert_eq!(format_duration_ms(-1), "0s");
    }

    #[test]
    fn budget_summary_reports_limits_and_remaining() {
        let mut goal = goal_with("active");
        goal.turns_used = 3;
        goal.max_turns = Some(20);
        goal.deadline_ms = Some(1_000_000);
        let summary = goal.budget_summary(940_000);
        assert!(summary.contains("turns 3/20"), "{summary}");
        assert!(summary.contains("1m0s left"), "{summary}");

        let unlimited = goal_with("active");
        let summary = unlimited.budget_summary(0);
        assert!(summary.contains("unlimited"));
    }

    #[test]
    fn budget_hint_is_empty_without_limits() {
        assert!(goal_with("active").budget_hint().is_empty());
    }

    #[test]
    fn splits_title_and_notes() {
        let (title, notes) = split_title_notes("ship it --notes fast and safe");
        assert_eq!(title, "ship it");
        assert_eq!(notes.as_deref(), Some("fast and safe"));

        let (title, notes) = split_title_notes("just a title");
        assert_eq!(title, "just a title");
        assert!(notes.is_none());
    }

    #[test]
    fn done_when_is_reported_and_fingerprinted() {
        let mut goal = goal_with("active");
        goal.done_when = Some("board open == 0".into());
        assert!(render_status_text(Some(&goal)).contains("definition of done: board open == 0"));
        assert_eq!(goal_summary_json(&goal)["doneWhen"], "board open == 0");
        assert!(fingerprint(&goal).contains("board open == 0"));
    }

    #[test]
    fn status_text_without_goal_is_actionable() {
        assert!(render_status_text(None).contains("/goal start"));
    }

    #[test]
    fn apply_limits_distinguishes_clear_from_untouched() {
        let mut base = goal_with("active");
        base.max_turns = Some(10);
        base.deadline_ms = Some(5_000);

        // Only turns given → deadline preserved, turns replaced.
        let updated = apply_limits(&base, 1_000, Some(Some(20)), None);
        assert_eq!(updated.max_turns, Some(20));
        assert_eq!(updated.deadline_ms, Some(5_000));

        // Explicit clear → that field dropped, the other preserved.
        let updated = apply_limits(&base, 1_000, Some(None), None);
        assert_eq!(updated.max_turns, None);
        assert_eq!(updated.deadline_ms, Some(5_000));

        // Relative deadline resolves against `now`.
        let updated = apply_limits(&base, 1_000, None, Some(Some(3_000)));
        assert_eq!(updated.deadline_ms, Some(4_000));
        assert_eq!(updated.max_turns, Some(10));

        // Nothing passed → unchanged.
        let updated = apply_limits(&base, 1_000, None, None);
        assert_eq!(updated.max_turns, Some(10));
        assert_eq!(updated.deadline_ms, Some(5_000));
    }
}
