//! Right-rail observability: the plan and one box per node.
//!
//! `plan`/`dispatch` used to be a black box — a wave could run for minutes
//! while the transcript showed a single "● dispatch" line. The host's component
//! registry already renders `sidebar.right` panels in `order` on the right rail
//! (`crates/kiqi-cli/src/interactive_tui/panels.rs`), so the guest publishes:
//!
//! * one `plan` panel — the node list with status marks, and
//! * one bordered box per node — its state, elapsed time and the tail of its
//!   output, refreshed at a bounded rate.
//!
//! Every call is best-effort: a host without a UI (headless, unit tests) answers
//! `denied`/`failed`, and the wave must keep running regardless.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use kiqi_wasm_bindings::bindings::kiqi::extension::ui;
use serde_json::{json, Value};

use crate::{Graph, Status};

const SLOT: &str = "sidebar.right";
const PLAN_ID: &str = "orchestrator.plan";
const PLAN_ORDER: i32 = 10;
const NODE_ORDER_BASE: i32 = 20;
const PANEL_WIDTH: usize = 38;
const PLAN_HEIGHT: usize = 16;
const NODE_HEIGHT: usize = 10;
const NODE_TAIL: usize = 8;
/// Below this terminal width the host keeps the chat column instead of a rail.
const MIN_SCREEN_WIDTH: i32 = 80;
/// How often a node box may be rewritten while its child streams.
const REFRESH: Duration = Duration::from_millis(400);

fn panel(args: Value) {
    // The name is a routing slot in the host's component registry, not a
    // component type; `panel` is the generic text panel every slot uses.
    let _ = ui::component("panel", &args.to_string());
}

fn publish(id: &str, order: i32, title: &str, lines: &[String], height: usize) {
    panel(json!({
        "op": "register",
        "id": id,
        "type": "text",
        "slot": SLOT,
        "order": order,
        "width": PANEL_WIDTH,
        "maxHeight": height,
        "border": true,
        "minScreenWidth": MIN_SCREEN_WIDTH,
    }));
    panel(json!({
        "op": "update",
        "id": id,
        "title": title,
        "text": lines.join("\n"),
    }));
}

fn remove(id: &str) {
    panel(json!({ "op": "remove", "id": id }));
    tracked().lock().unwrap_or_else(|p| p.into_inner()).remove(id);
}

/// Node boxes currently published, so a plan rewrite can drop the stale ones.
fn tracked() -> &'static Mutex<BTreeSet<String>> {
    static TRACKED: LazyLock<Mutex<BTreeSet<String>>> =
        LazyLock::new(|| Mutex::new(BTreeSet::new()));
    &TRACKED
}

struct Tail {
    lines: VecDeque<String>,
    last: Instant,
    started: Instant,
}

fn tails() -> &'static Mutex<HashMap<String, Tail>> {
    static TAILS: LazyLock<Mutex<HashMap<String, Tail>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &TAILS
}

fn status_mark(status: Status) -> &'static str {
    match status {
        Status::Done => "✔",
        Status::Running => "▶",
        Status::Failed => "✖",
        Status::Skipped => "–",
        Status::Pending => "·",
    }
}

fn status_word(status: Status) -> &'static str {
    match status {
        Status::Done => "done",
        Status::Running => "running",
        Status::Failed => "failed",
        Status::Skipped => "skipped",
        Status::Pending => "pending",
    }
}

/// `12s` / `1m20s` / `1h02m` — short enough for a 38-column box.
fn elapsed_label(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Strip ANSI escapes and control characters: the panel renders plain text and
/// a child process streams colour codes it never expected to reach a border.
fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: `ESC [` … final byte in @-~. Anything else: drop one char.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_control() {
            continue;
        }
        out.push(c);
    }
    out.trim_end().to_string()
}

fn node_lines(id: &str) -> Vec<String> {
    let tails = tails().lock().unwrap_or_else(|p| p.into_inner());
    tails
        .get(id)
        .map(|tail| tail.lines.iter().cloned().collect())
        .unwrap_or_default()
}

/// Publish the plan panel and drop boxes for nodes the plan no longer has.
pub(crate) fn sync_plan(graph: &Graph) {
    let done = graph
        .nodes
        .iter()
        .filter(|node| node.status == Status::Done)
        .count();
    let mut lines = vec![format!("{done}/{} done", graph.nodes.len())];
    for node in &graph.nodes {
        lines.push(format!("{} {}", status_mark(node.status), node.id));
    }
    publish(PLAN_ID, PLAN_ORDER, "plan", &lines, PLAN_HEIGHT);

    let live: BTreeSet<String> = graph
        .nodes
        .iter()
        .map(|node| format!("orchestrator.node.{}", node.id))
        .collect();
    let stale: Vec<String> = tracked()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter(|id| !live.contains(*id))
        .cloned()
        .collect();
    for id in stale {
        remove(&id);
    }
}

pub(crate) fn node_started(id: &str, index: usize) {
    let panel_id = format!("orchestrator.node.{id}");
    tracked()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(panel_id.clone());
    tails()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            id.to_string(),
            Tail {
                lines: VecDeque::new(),
                last: Instant::now(),
                started: Instant::now(),
            },
        );
    publish(
        &panel_id,
        NODE_ORDER_BASE + index as i32,
        &format!("{id} · running"),
        &["(starting)".to_string()],
        NODE_HEIGHT,
    );
}

/// Append one output line and refresh the box at most once per [`REFRESH`].
pub(crate) fn node_line(id: &str, line: &str) {
    let mut tails = tails().lock().unwrap_or_else(|p| p.into_inner());
    let Some(tail) = tails.get_mut(id) else {
        return;
    };
    let line = clean(line);
    if line.is_empty() {
        return;
    }
    tail.lines.push_back(line);
    while tail.lines.len() > NODE_TAIL {
        tail.lines.pop_front();
    }
    if tail.last.elapsed() < REFRESH {
        return;
    }
    tail.last = Instant::now();
    let lines: Vec<String> = tail.lines.iter().cloned().collect();
    drop(tails);
    publish(
        &format!("orchestrator.node.{id}"),
        NODE_ORDER_BASE,
        &format!("{id} · running"),
        &lines,
        NODE_HEIGHT,
    );
}

/// Close a node's box with its final state and how long it took.
pub(crate) fn node_finished(id: &str, status: Status) {
    let lines = node_lines(id);
    let elapsed = tails()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(id)
        .map(|tail| tail.started.elapsed())
        .unwrap_or_default();
    publish(
        &format!("orchestrator.node.{id}"),
        NODE_ORDER_BASE,
        &format!("{id} · {} {}", status_word(status), elapsed_label(elapsed)),
        &lines,
        NODE_HEIGHT,
    );
}

/// Session teardown: the rail outlives the guest, so the boxes are removed
/// explicitly rather than left to show a dead wave.
pub(crate) fn clear_all() {
    panel(json!({ "op": "remove", "id": PLAN_ID }));
    let ids: Vec<String> = tracked()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .cloned()
        .collect();
    for id in ids {
        panel(json!({ "op": "remove", "id": id }));
    }
    tracked().lock().unwrap_or_else(|p| p.into_inner()).clear();
    tails().lock().unwrap_or_else(|p| p.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::{clean, elapsed_label, status_mark};
    use crate::Status;
    use std::time::Duration;

    #[test]
    fn elapsed_labels_stay_short() {
        assert_eq!(elapsed_label(Duration::from_secs(9)), "9s");
        assert_eq!(elapsed_label(Duration::from_secs(80)), "1m20s");
        assert_eq!(elapsed_label(Duration::from_secs(3720)), "1h02m");
    }

    #[test]
    fn output_is_cleaned_for_a_bordered_box() {
        assert_eq!(clean("\u{1b}[32m✔ passed\u{1b}[0m"), "✔ passed");
        // Tabs are control characters: a bordered row renders them as nothing
        // rather than letting the terminal re-align the box.
        assert_eq!(clean("plain\ttabs\r"), "plaintabs");
        assert_eq!(clean("\u{1b}[1;31mred"), "red");
    }

    #[test]
    fn every_state_has_its_own_mark() {
        let marks: Vec<&str> = [
            Status::Pending,
            Status::Running,
            Status::Done,
            Status::Failed,
            Status::Skipped,
        ]
        .iter()
        .map(|status| status_mark(*status))
        .collect();
        assert_eq!(marks, vec!["·", "▶", "✔", "✖", "–"]);
    }
}
