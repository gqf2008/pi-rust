//! Host-side view of the goal plugin's state.
//!
//! The goal plugin owns `.ki/goals.json` and injects the active goal into the
//! model context on `TurnEnd`. That makes the agent goal-aware, but it cannot
//! start a new run: if the model ends a run while the goal is still active, the
//! session would sit idle. This module lets the TUI detect that state and
//! continue the goal automatically.
//!
//! The file is read-only here; the plugin remains the writer. A missing or
//! malformed file means "no active goal", never an error.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Absolute path of the goal state file, relative to the session cwd (the same
/// path the plugin uses).
pub(crate) const GOAL_FILE: &str = ".ki/goals.json";

/// Safety net for an *unbounded* goal. A goal with `maxTurns`/`maxMinutes` is
/// already bounded by the plugin; an unbounded one must not loop forever in the
/// host. User input or a new goal resets the chain.
pub(crate) const MAX_GOAL_AUTO_RUNS: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveGoal {
    pub title: String,
    pub started_at: i64,
    pub turns_used: u32,
    pub max_turns: Option<u32>,
    pub deadline_ms: Option<i64>,
    /// Explicit stop condition. The host currently recognizes the literal
    /// `"board-open-0"`: it runs `walgit collab board` at the AgentEnd boundary
    /// and skips auto-continue when the board has no open cards. Unknown
    /// conditions are documented to the model but not evaluated by the host.
    pub done_when: Option<String>,
}

impl ActiveGoal {
    /// A stable key for one goal: a new goal (title or start time changed)
    /// resets the host's continuation counter.
    pub(crate) fn key(&self) -> String {
        format!("{}@{}", self.title, self.started_at)
    }

    /// Whether the goal still has budget left. The plugin enforces the same
    /// limits at `TurnEnd`; checking here avoids starting a run that the plugin
    /// would immediately pause.
    pub(crate) fn can_continue(&self, now_ms: i64) -> bool {
        if self.max_turns.is_some_and(|max| self.turns_used >= max) {
            return false;
        }
        if self.deadline_ms.is_some_and(|deadline| now_ms >= deadline) {
            return false;
        }
        true
    }

    /// The synthetic input that asks the agent to keep working. The goal itself
    /// is already in context (the plugin injected it); this is the trigger.
    pub(crate) fn continuation_prompt(&self) -> String {
        format!(
            "[goal continuation]\n\
             Continue advancing the active goal: {}.\n\
             Take the next useful step now. Stop only when the goal is complete, paused, \
             or you need authorization from the user.\n",
            self.title
        )
    }
}

/// Read the active goal from `cwd/.ki/goals.json`. `None` when the file is
/// absent/malformed, there is no active goal, or it is paused/completed.
pub(crate) fn active_goal(cwd: &Path) -> Option<ActiveGoal> {
    let text = std::fs::read_to_string(cwd.join(GOAL_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let active = value.get("active")?.as_object()?;
    if active.get("status").and_then(serde_json::Value::as_str) != Some("active") {
        return None;
    }
    Some(ActiveGoal {
        title: active.get("title")?.as_str()?.to_string(),
        started_at: active
            .get("started_at")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        turns_used: active
            .get("turns_used")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        max_turns: active
            .get("max_turns")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as u32),
        deadline_ms: active
            .get("deadline_ms")
            .and_then(serde_json::Value::as_i64),
        done_when: active
            .get("done_when")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

/// Whether a `walgit collab board` rendering says there are no open cards.
/// The board prints headings such as `== open (0) ==`; a substring check would
/// also match `10`, so match the trimmed line exactly.
pub(crate) fn board_has_no_open_cards(stdout: &str) -> bool {
    stdout.lines().any(|line| line.trim() == "== open (0) ==")
}

pub(crate) fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_goal(dir: &Path, value: serde_json::Value) {
        std::fs::create_dir_all(dir.join(".ki")).unwrap();
        std::fs::write(dir.join(GOAL_FILE), value.to_string()).unwrap();
    }

    #[test]
    fn missing_or_malformed_file_means_no_goal() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(active_goal(tmp.path()).is_none());
        std::fs::create_dir_all(tmp.path().join(".ki")).unwrap();
        std::fs::write(tmp.path().join(GOAL_FILE), "{not json").unwrap();
        assert!(active_goal(tmp.path()).is_none());
    }

    #[test]
    fn active_goal_is_read_and_paused_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        write_goal(
            tmp.path(),
            serde_json::json!({
                "active": {
                    "title": "ship it",
                    "status": "active",
                    "started_at": 10,
                    "turns_used": 2,
                    "max_turns": 5,
                    "deadline_ms": null
                }
            }),
        );
        let goal = active_goal(tmp.path()).expect("active goal");
        assert_eq!(goal.title, "ship it");
        assert_eq!(goal.turns_used, 2);
        assert_eq!(goal.max_turns, Some(5));
        assert!(goal.can_continue(0));

        write_goal(
            tmp.path(),
            serde_json::json!({
                "active": { "title": "done", "status": "paused", "started_at": 10 }
            }),
        );
        assert!(active_goal(tmp.path()).is_none());
    }

    #[test]
    fn turn_and_time_budgets_stop_continuation() {
        let goal = ActiveGoal {
            title: "bounded".into(),
            started_at: 0,
            turns_used: 5,
            max_turns: Some(5),
            deadline_ms: Some(1_000),
            done_when: None,
        };
        assert!(!goal.can_continue(0));
        let goal = ActiveGoal {
            title: "timed".into(),
            started_at: 0,
            turns_used: 0,
            max_turns: None,
            deadline_ms: Some(1_000),
            done_when: None,
        };
        assert!(goal.can_continue(999));
        assert!(!goal.can_continue(1_000));
    }

    #[test]
    fn active_goal_reads_the_definition_of_done() {
        let tmp = tempfile::tempdir().unwrap();
        write_goal(
            tmp.path(),
            serde_json::json!({
                "active": {
                    "title": "finish the board",
                    "status": "active",
                    "started_at": 10,
                    "done_when": "board-open-0"
                }
            }),
        );
        let goal = active_goal(tmp.path()).expect("active goal");
        assert_eq!(goal.done_when.as_deref(), Some("board-open-0"));
    }

    #[test]
    fn board_open_zero_marker_is_matched_exactly() {
        assert!(board_has_no_open_cards("== open (0) ==\n"));
        assert!(!board_has_no_open_cards("== open (1) ==\n"));
        assert!(!board_has_no_open_cards(" == open (0) == trailing\n"));
    }

    #[test]
    fn continuation_prompt_names_the_goal_and_the_stop_rule() {
        let goal = ActiveGoal {
            title: "finish the board".into(),
            started_at: 0,
            turns_used: 0,
            max_turns: None,
            deadline_ms: None,
            done_when: None,
        };
        let prompt = goal.continuation_prompt();
        assert!(prompt.contains("finish the board"));
        assert!(prompt.contains("Stop only when the goal is complete"));
        assert!(prompt.contains("authorization"));
    }
}
