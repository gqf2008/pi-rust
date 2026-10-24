#![forbid(unsafe_code)]

//! `kiqi-goal` ported to the wasm ABI (0.7.0).
//!
//! Goal mode records one persistent, context-injected objective per project and
//! lets the model advance it across turns: the `goal` tool (eight actions), the
//! `/goal` command, and a `TurnEnd` hook that counts turns, enforces the budget
//! and re-injects the active goal.
//!
//! State lives in `<project>/.ki/goals.json` — the compile-time `GOAL_FILE` and
//! the exact shape the compiled `kiqi-orchestrator` reads, so an active goal
//! started here turns the compiled orchestration guidance on. (The `fs` grant's
//! scope is now the project tree — a grant-model change, not an ABI one; the
//! ABI stays 0.7.1. Before the scope existed the guest kept a
//! `{ "<project path>": Goals }` map in its own data directory, which the
//! compiled orchestrator could not see.)
//!
//! Two things still differ from the compile-time plugin, and each is stated
//! rather than hidden (see the README for the full list):
//!
//! * **`TurnEnd` arrives untyped.** The WIT event enum has no turn-end tag; the
//!   host maps `EventTag::TurnEnd` to `event::other` with kind `"TurnEnd"`, so
//!   the hook string-matches `other.kind`.
//! * **Injection payload.** `inject.append-entry` forwards the parsed entry
//!   straight to `InjectApi::append_message`, so the guest hands it the bare
//!   custom `AgentMessage` JSON — the same value the compile-time `CapsHost`
//!   passes to `append_message`, *not* the `{"message": ...}` wrapper the C-ABI
//!   host builds before the `AppendEntry` runtime action.
//!
//! Everything else — the budget arithmetic, the action dispatch, the command
//! grammar and the fingerprint dedupe — is the compile-time logic, kept in the
//! guest.

use std::sync::{Mutex, OnceLock};

use kiqi_plugin_guest::bindings::exports::kiqi::plugin::plugin_api::Guest;
use kiqi_plugin_guest::bindings::kiqi::plugin::{fs, inject, session_read, types};
use serde_json::{json, Value};

const ID: &str = "kiqi-goal";
/// Project-relative path of the state file — the compile-time `kiqi-goal`
/// contract (`GOAL_FILE`). The `fs` grant's scope is the project tree, so this
/// path resolves under the session's directory.
const PROJECT_FILE: &str = ".ki/goals.json";
/// The pre-scope store: the `{ "<project>": Goals }` map the guest used to keep
/// in its own data directory under `fs` scope `plugin-data`. Read-only, and only
/// when the current `fs` root makes it reachable (see the README); used to
/// migrate a project's entry into [`PROJECT_FILE`] on first use.
const LEGACY_STORE_FILE: &str = "goals.json";
/// The custom-message type stored in the generic AgentMessage payload.
const GOAL_CUSTOM_TYPE: &str = "goal";
const MAX_TITLE_CHARS: usize = 500;
const MAX_NOTES_CHARS: usize = 20_000;
/// The host maps `EventTag::TurnEnd` to `other.kind`; this is the exact string
/// it uses (`format!("{tag:?}")` on the host's `EventTag`).
const TURN_END_KIND: &str = "TurnEnd";

const DESCRIPTION: &str = "Codex-style goal mode: record one persistent, context-injected objective per project and let the agent advance it across turns. Actions: start, update, pause, resume, complete, limit, show, list.";
const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["start","update","pause","resume","complete","limit","show","list"]},"title":{"type":"string","description":"Goal title (required for start)"},"notes":{"type":"string","description":"Goal notes or progress update (start/update)"},"status":{"type":"string","enum":["active","paused"],"description":"Explicit status change (update)"},"maxTurns":{"type":"integer","minimum":1,"description":"Optional turn budget (start/limit)"},"maxMinutes":{"type":"integer","minimum":1,"description":"Optional wall-clock budget in minutes (start/limit)"},"doneWhen":{"type":"string","description":"Definition of done: the condition that must hold before the agent marks the goal complete; the host currently evaluates the literal \"board-open-0\" at AgentEnd"},"doneWhenCmd":{"type":"string","description":"Optional host-run command evaluated at AgentEnd with a short timeout; exit 0 marks the goal done (runs under the same shell sandbox as bash tools)"}},"required":["action"]}"#;
const COMMAND: &str = "goal";

/// Last injected goal-state fingerprint (title + status + notes). Prevents
/// re-injecting the same status on every `TurnEnd` (transcript bloat).
static LAST_INJECTED: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn last_injected() -> &'static Mutex<Option<String>> {
    LAST_INJECTED.get_or_init(|| Mutex::new(None))
}

// ---------------------------------------------------------------------------
// Host seam
// ---------------------------------------------------------------------------

/// The one host operation goal needs. It goes through `inject.append-entry`,
/// which the host forwards verbatim to `InjectApi::append_message`.
trait GoalHost {
    fn append_message(&self, message: &Value) -> Result<(), String>;
}

struct WasmHost;

impl GoalHost for WasmHost {
    fn append_message(&self, message: &Value) -> Result<(), String> {
        // The host parses this string and passes the parsed value straight to
        // `InjectApi::append_message`, so it is the bare custom AgentMessage —
        // `{"message": ...}` would make the host's `append_message` wrap it a
        // second time and the message entry would fail to parse.
        inject::append_entry(&message.to_string()).map_err(reason)
    }
}

fn reason(error: types::CapabilityError) -> String {
    match error {
        types::CapabilityError::Denied(reason) => format!("denied: {reason}"),
        types::CapabilityError::Failed(reason) => reason,
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Goal {
    title: String,
    status: String, // active | paused | complete
    notes: String,
    started_at: i64,
    updated_at: i64,
    /// Turns completed while the goal was active. Drives the `max_turns` budget.
    turns_used: u32,
    /// Turn budget. `None` = unlimited.
    max_turns: Option<u32>,
    /// Absolute epoch-ms deadline. `None` = unlimited.
    deadline_ms: Option<i64>,
    /// Explicit definition of done, injected as the stopping criterion. The
    /// host cannot evaluate arbitrary conditions; the agent must check it
    /// before calling `complete`.
    done_when: Option<String>,
    /// Optional host-run command evaluated at AgentEnd with a short timeout;
    /// exit 0 marks the goal done. Runs under the same shell sandbox as the
    /// bash tool, so it cannot rewrite the permission policy.
    done_when_cmd: Option<String>,
}

impl Goal {
    fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "status": self.status,
            "notes": self.notes,
            "started_at": self.started_at,
            "updated_at": self.updated_at,
            "turns_used": self.turns_used,
            "max_turns": self.max_turns,
            "deadline_ms": self.deadline_ms,
            "done_when": self.done_when,
            "done_when_cmd": self.done_when_cmd,
        })
    }

    /// Lenient parse, matching the compile-time `#[serde(default)]` contract:
    /// `title` is required, the budget fields default when missing so older
    /// files keep loading.
    fn from_json(value: &Value) -> Option<Goal> {
        Some(Goal {
            title: value.get("title")?.as_str()?.to_string(),
            status: value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("active")
                .to_string(),
            notes: value
                .get("notes")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            started_at: value.get("started_at").and_then(Value::as_i64).unwrap_or(0),
            updated_at: value.get("updated_at").and_then(Value::as_i64).unwrap_or(0),
            turns_used: value.get("turns_used").and_then(Value::as_u64).unwrap_or(0) as u32,
            max_turns: value
                .get("max_turns")
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            deadline_ms: value.get("deadline_ms").and_then(Value::as_i64),
            done_when: value
                .get("done_when")
                .and_then(Value::as_str)
                .map(str::to_string),
            done_when_cmd: value
                .get("done_when_cmd")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

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

#[derive(Clone, Debug, Default)]
struct Goals {
    active: Option<Goal>,
    archived: Vec<Goal>,
}

impl Goals {
    fn to_json(&self) -> Value {
        json!({
            "active": self.active.as_ref().map(Goal::to_json),
            "archived": self.archived.iter().map(Goal::to_json).collect::<Vec<_>>(),
        })
    }

    fn from_json(value: &Value) -> Option<Goals> {
        let active = value.get("active").and_then(Goal::from_json);
        let archived = value
            .get("archived")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Goal::from_json).collect())
            .unwrap_or_default();
        Some(Goals { active, archived })
    }
}

/// The session's working directory. Used only to find this project's entry in
/// the pre-scope store during migration — the project file itself is addressed
/// through the `fs` root, which the host scopes to the project tree.
fn project_key() -> String {
    session_read::info().cwd
}

/// Parse a `Goals` value, or `None` when the file holds something else — the
/// compile-time `read_goals` treated a malformed file as "no goals".
fn parse_goals(bytes: &[u8]) -> Option<Goals> {
    let text = std::str::from_utf8(bytes).ok()?;
    let value: Value = serde_json::from_str(text).ok()?;
    Goals::from_json(&value)
}

/// This project's entry in the pre-scope data-directory store, when that store
/// is reachable through the current `fs` root. Absent/unreadable/malformed, or
/// no entry for this project, is `None`.
fn legacy_entry() -> Option<Goals> {
    let bytes = fs::read(LEGACY_STORE_FILE).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let store: Value = serde_json::from_str(text).ok()?;
    store
        .as_object()?
        .get(&project_key())
        .and_then(Goals::from_json)
}

fn read_goals() -> Goals {
    match fs::read(PROJECT_FILE) {
        Ok(bytes) => parse_goals(&bytes).unwrap_or_default(),
        Err(_) => {
            // No project file yet. Surface any pre-scope state *if* the old
            // data-directory store is reachable through this `fs` root; in the
            // shipped layout it is not (the data directory sits outside the
            // project tree), so this is normally a no-op and the guest starts
            // from nothing — see the README's "Pre-scope migration".
            match legacy_entry() {
                Some(goals) => {
                    let _ = write_goals(&goals);
                    goals
                }
                None => Goals::default(),
            }
        }
    }
}

fn write_goals(goals: &Goals) -> Result<(), String> {
    // The `fs` capability has no `rename`, so this truncate-writes in place; the
    // file now holds one project's goals, so a torn write no longer takes every
    // project's state with it (see the README).
    let text = goals.to_json().to_string();
    fs::write(PROJECT_FILE, &text.into_bytes()).map_err(reason)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Goal-state fingerprint used to dedupe injections. Deliberately excludes the
/// live counters (`turns_used`) so a running budget does not re-inject each turn.
fn fingerprint(goal: &Goal) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        goal.title,
        goal.status,
        goal.notes,
        goal.done_when.as_deref().unwrap_or(""),
        goal.done_when_cmd.as_deref().unwrap_or("")
    )
}

/// Mark the current active-goal status as already injected.
fn remember_injected(fingerprint: Option<String>) {
    let mut guard = last_injected()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = fingerprint;
}

/// Should we inject now? False when the same goal-state was already appended.
fn should_inject(fingerprint: Option<&str>) -> bool {
    let guard = last_injected()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}

// ---------------------------------------------------------------------------
// Context injection
// ---------------------------------------------------------------------------

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
                "doneWhen": goal.done_when,
                "doneWhenCmd": goal.done_when_cmd,
                "startedAt": goal.started_at,
                "updatedAt": goal.updated_at,
            }
        },
        "timestamp": goal.updated_at,
    });
    host.append_message(&message)
}

/// The orchestration nudge injected with an *active* goal. Emitted only when
/// the orchestrator's tools are actually on the session's tool face — naming
/// tools that are not installed would send the model hunting for nothing.
const ORCHESTRATION_HINT: &str =
    "推进方式：用 `plan` 写 DAG、`dispatch` 跑、节点交给 `subagent`。";

fn orchestration_hint() -> Option<&'static str> {
    let tools = session_read::active_tools();
    let present = |name: &str| tools.iter().any(|tool| tool == name);
    (present("plan") && present("dispatch") && present("subagent")).then_some(ORCHESTRATION_HINT)
}

/// Append the goal's current state as a generic custom message. The host stores
/// it as an ordinary message entry and the generic context builder forwards it
/// to the model; no goal-specific host projector is required.
fn append_goal_custom_message(host: &dyn GoalHost, goal: &Goal) -> Result<(), String> {
    // One `match` yields both the tag and whether the orchestration nudge may
    // follow, so adding a status cannot pick up a tag while skipping the guard.
    let (tag, hint_allowed) = match goal.status.as_str() {
        "active" => ("🎯 Active goal", true),
        "paused" => ("⏸️ Paused goal", false),
        "complete" => ("✅ Goal completed", false),
        // Unknown/legacy status: keep the neutral tag, never the nudge.
        _ => ("🎯 Active goal", false),
    };
    let notes = goal.notes.trim();
    let budget = goal.budget_hint();
    let text = if notes.is_empty() {
        format!("{tag}: {}.{budget}", goal.title)
    } else {
        format!("{tag}: {} — {}.{budget}", goal.title, notes)
    };
    let mut text = text;
    if let Some(done_when) = goal.done_when.as_deref() {
        text.push_str(&format!("\nDefinition of done: {done_when}"));
    }
    if let Some(done_when_cmd) = goal.done_when_cmd.as_deref() {
        text.push_str(&format!(
            "\nDefinition of done (host-run, exit 0 = done): {done_when_cmd}"
        ));
    }
    let nudge = if hint_allowed {
        orchestration_hint()
    } else {
        None
    };
    let text = match nudge {
        Some(nudge) => format!("{text}\n{nudge}"),
        None => text,
    };
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

/// A host-run done command. Kept short and empty-free; the host runs it
/// through the same shell sandbox as the bash tool with a short timeout, so
/// it can evaluate `walgit collab board` or a test suite but cannot rewrite
/// the permission policy.
fn validate_done_when_cmd(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    if value.chars().count() > 1000 {
        return Err("doneWhenCmd must not exceed 1000 characters".into());
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
    done_when_cmd: Option<&str>,
) -> Result<Goal, String> {
    let title = validate_title(title)?;
    let notes = validate_notes(notes)?;
    let done_when = done_when
        .map(validate_done_when)
        .transpose()?
        .filter(|value| !value.is_empty());
    let done_when_cmd = done_when_cmd
        .map(validate_done_when_cmd)
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
        done_when_cmd,
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
/// `Some(None)` clears it; `Some(Some(v))` sets it (a relative ms value for the
/// deadline, resolved against `now`).
fn apply_limits(
    goal: &Goal,
    now: i64,
    max_turns: Option<Option<u32>>,
    deadline_ms: Option<Option<i64>>,
) -> Goal {
    let mut updated = goal.clone();
    if let Some(max_turns) = max_turns {
        updated.max_turns = max_turns.filter(|value| *value > 0);
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

fn show_goal() -> Result<String, String> {
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

fn list_goals() -> Result<String, String> {
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
    params
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as u32)
}

fn start_goal(host: &dyn GoalHost, params: &Value) -> Result<String, String> {
    let title = params
        .get("title")
        .and_then(Value::as_str)
        .ok_or("goal title is required")?;
    let notes = params.get("notes").and_then(Value::as_str).unwrap_or("");
    let max_turns = param_u32(params, "maxTurns").filter(|value| *value > 0);
    let deadline_ms = param_u32(params, "maxMinutes")
        .filter(|value| *value > 0)
        .map(|minutes| now_ms().saturating_add((minutes as i64) * 60_000));
    let done_when = params.get("doneWhen").and_then(Value::as_str);
    let done_when_cmd = params.get("doneWhenCmd").and_then(Value::as_str);
    let goal = op_start(
        host,
        title,
        notes,
        max_turns,
        deadline_ms,
        done_when,
        done_when_cmd,
    )?;
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

fn update_goal(host: &dyn GoalHost, params: &Value) -> Result<String, String> {
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

fn status_goal(host: &dyn GoalHost, status: &str) -> Result<String, String> {
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

fn complete_goal(host: &dyn GoalHost) -> Result<String, String> {
    let goal = op_complete(host)?;
    let text = format!(
        "## ✅ Goal complete\n\n**{}**  \n\nArchived. The goal is no longer injected into context.",
        goal.title
    );
    Ok(tool_envelope("complete", text, false, Some(&goal)))
}

fn limit_goal(host: &dyn GoalHost, params: &Value) -> Result<String, String> {
    let max_turns = params
        .get("maxTurns")
        .and_then(Value::as_u64)
        .map(|value| (value as u32).max(1));
    let max_minutes = params
        .get("maxMinutes")
        .and_then(Value::as_u64)
        .map(|value| (value as i64) * 60_000);
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

fn goal(host: &dyn GoalHost, params: &Value) -> Result<String, String> {
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
// TurnEnd hook — count turns, enforce budgets, inject the active goal
// ---------------------------------------------------------------------------

/// The `TurnEnd` body. `TurnEnd` carries no payload.
fn turn_end(host: &dyn GoalHost) -> i32 {
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

/// Extract `title` / `--notes` from a free-form string.
fn split_title_notes(rest: &str) -> (String, Option<String>) {
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

fn run_command(host: &dyn GoalHost, args: &str) -> String {
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
            op_start(host, &title, notes.as_deref().unwrap_or(""), None, None, None, None).map(|goal| {
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
            op_start(host, &title, notes.as_deref().unwrap_or(""), None, None, None, None)
                .map(|goal| format!("🎯 Goal started: {}", goal.title))
        }
    };

    match result {
        Ok(text) => text,
        Err(error) => format!("⚠️ {error}"),
    }
}

// ---------------------------------------------------------------------------
// Exported plugin surface
// ---------------------------------------------------------------------------

struct GoalPlugin;

impl Guest for GoalPlugin {
    /// No selector/editor/prompt flow exists in this plugin yet; a reply is
    /// refused rather than silently dropped.
    fn continue_command(
        _name: String,
        _flow_id: String,
        _reply: types::CommandReply,
    ) -> Result<types::CommandResult, types::PluginError> {
        Err(types::PluginError::Failed(
            "this plugin declares no command continuations".to_string(),
        ))
    }

    fn id() -> String {
        ID.to_string()
    }

    fn shutdown() {}

    fn list_tools() -> Vec<types::ToolSpec> {
        vec![types::ToolSpec {
            name: "goal".to_string(),
            description: DESCRIPTION.to_string(),
            parameters_schema: SCHEMA.to_string(),
        }]
    }

    fn call_tool(name: String, arguments: String) -> Result<types::ToolResult, types::PluginError> {
        if name != "goal" {
            return Err(types::PluginError::UnknownTool(name));
        }
        let params: Value = serde_json::from_str(&arguments)
            .map_err(|error| types::PluginError::InvalidArguments(error.to_string()))?;
        let envelope = goal(&WasmHost, &params).map_err(types::PluginError::InvalidArguments)?;
        // The envelope rides the tool channel: the text block is what the model
        // reads, `details` is the structured metadata that survives fork/switch.
        let parsed: Value = serde_json::from_str(&envelope).unwrap_or(Value::String(envelope));
        let text = parsed
            .get("content")
            .and_then(|content| content.get(0))
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| parsed.to_string());
        let details = parsed.get("details").cloned().unwrap_or(Value::Null);
        Ok(types::ToolResult {
            blocks: vec![types::ContentBlock::Text(text)],
            is_error: false,
            details: details.to_string(),
        })
    }

    fn list_commands() -> Vec<types::CommandSpec> {
        vec![types::CommandSpec {
            name: COMMAND.to_string(),
            description: "Show, start, pause, stop, or budget the active goal".to_string(),
        }]
    }

    fn run_command(name: String, arguments: String) -> Result<types::CommandResult, types::PluginError> {
        if name != COMMAND {
            return Err(types::PluginError::UnknownTool(name));
        }
        Ok(types::CommandResult {
            flow_id: String::new(),
            body: types::CommandResultBody::Message(run_command(&WasmHost, &arguments)),
        })
    }

    fn list_flags() -> Vec<types::FlagSpec> {
        Vec::new()
    }

    fn list_shortcuts() -> Vec<types::ShortcutSpec> {
        Vec::new()
    }

    /// `TurnEnd` is where the goal notice is injected and where the turn budget
    /// is charged. The WIT event enum has no turn-end tag, so the host delivers
    /// it as `other` with kind `"TurnEnd"`.
    fn on_event(event: types::Event) -> types::EventAction {
        if let types::Event::Other(other) = &event {
            if other.kind == TURN_END_KIND {
                turn_end(&WasmHost);
            }
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

kiqi_plugin_guest::bindings::export!(GoalPlugin with_types_in kiqi_plugin_guest::bindings);

// The 0.8.0 contribution exports are mandatory world exports; deny them all.
kiqi_plugin_guest::deny_contribution_exports!(GoalPlugin);
