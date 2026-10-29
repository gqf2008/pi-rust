//! The durable half of ↑/↓ recall.
//!
//! `TuiState::history` lives and dies with the process, so every new `ki` used
//! to start with an empty memo and the previous session's prompts had to be
//! retyped. This module keeps them on disk: one JSON string per line, oldest
//! first, next to the rest of the agent state.
//!
//! JSON (not plain lines) because a prompt may be several lines — `Shift+Enter`
//! is a newline in the editor — and a line-per-entry file would split one
//! prompt into several entries.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Kept on disk: the same bound the in-memory list uses.
pub(super) const MAX_ENTRIES: usize = 100;

/// `<agent_dir>/input-history.jsonl`, or `None` when the agent directory is not
/// resolvable (headless runs without a home, mostly).
fn history_path() -> Option<PathBuf> {
    crate::config::agent_dir()
        .ok()
        .map(|dir| dir.join("input-history.jsonl"))
}

/// The recorded prompts, newest first, which is the order `navigate_history`
/// expects.
pub(super) fn load() -> Vec<String> {
    history_path()
        .map(|path| load_from(&path))
        .unwrap_or_default()
}

/// Append one submitted prompt. Best-effort: a read-only or missing agent
/// directory must never fail a submit.
pub(super) fn append(entry: &str) {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some(path) = history_path() {
        append_to(&path, trimmed);
    }
}

fn load_from(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    let overflow = lines.len() > MAX_ENTRIES;
    let tail = &lines[lines.len().saturating_sub(MAX_ENTRIES)..];
    let mut entries: Vec<String> = tail
        .iter()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .collect();
    entries.reverse();
    // A prompt sent again in the next session must not appear twice in a row.
    entries.dedup_by(|newer, older| newer == older);
    if overflow {
        rewrite(path, &entries);
    }
    entries
}

fn append_to(path: &Path, entry: &str) {
    if entry.trim().is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let _ = file.write_all(line.as_bytes());
        let _ = file.write_all(b"\n");
    }
}

/// Compact the file down to `entries` (newest first), oldest first on disk.
fn rewrite(path: &Path, entries: &[String]) {
    let mut body = String::new();
    for entry in entries.iter().rev() {
        if let Ok(line) = serde_json::to_string(entry) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    let _ = std::fs::write(path, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that does not collide with a parallel test.
    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "ki-input-history-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prompts_survive_a_restart_newest_first() {
        let dir = scratch("restart");
        let path = dir.join("input-history.jsonl");
        append_to(&path, "first");
        append_to(&path, "second");

        // A new process reads them back: newest first, which is what ↑ walks.
        assert_eq!(load_from(&path), vec!["second", "first"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn multi_line_prompts_and_unicode_round_trip() {
        let dir = scratch("multiline");
        let path = dir.join("input-history.jsonl");
        append_to(&path, "line one\nline two");
        append_to(&path, "修复 ask_user 的超时");

        assert_eq!(
            load_from(&path),
            vec!["修复 ask_user 的超时", "line one\nline two"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn duplicates_and_blanks_are_dropped() {
        let dir = scratch("dupes");
        let path = dir.join("input-history.jsonl");
        append_to(&path, "same");
        append_to(&path, "  ");
        append_to(&path, "same");
        append_to(&path, "other");

        assert_eq!(load_from(&path), vec!["other", "same"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_file_keeps_only_the_newest_entries() {
        let dir = scratch("cap");
        let path = dir.join("input-history.jsonl");
        for index in 0..(MAX_ENTRIES + 7) {
            append_to(&path, &format!("prompt {index}"));
        }
        let entries = load_from(&path);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(entries[0], format!("prompt {}", MAX_ENTRIES + 6));
        assert_eq!(entries[MAX_ENTRIES - 1], "prompt 7");
        // The overflow is compacted away, not just ignored on read.
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(lines, MAX_ENTRIES);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
