//! The work view: what the agents are doing, over the transcript.
//!
//! Ctrl+T overlays this on the output area (the input stays visible). It is not
//! a second transcript: the point is to *watch work* — the todo list, one pane
//! per subagent with its state, elapsed time, current tool and last words, and
//! a summary line. With nothing running it says so rather than showing empty
//! panes. Toggling it off restores the normal scrolling transcript.

use super::*;

/// One subagent pane's lines, without borders.
fn pane_rows(state: &TuiState, node: &NodeRow, width: usize) -> Vec<String> {
    let elapsed = human_ms(node.elapsed_ms);
    let budget = if node.budget_ms > 0 {
        human_ms(node.budget_ms)
    } else {
        "—".to_string()
    };
    let mut rows = Vec::new();
    match node.state.as_str() {
        "running" => {
            rows.push(format!("state   running {elapsed} / {budget}"));
            if node.budget_ms > 0 {
                let fraction = (node.elapsed_ms as f64 / node.budget_ms as f64).clamp(0.0, 1.0);
                let filled = (fraction * 16.0).round() as usize;
                let bar: String = (0..16)
                    .map(|cell| if cell < filled { '█' } else { '░' })
                    .collect();
                rows.push(format!("{bar} {:>3.0}%", fraction * 100.0));
            }
        }
        other => rows.push(format!("state   {other} {elapsed}")),
    }
    if let Some(tool) = &node.tool {
        rows.push(format!("tool    {tool}"));
    }
    if let Some(text) = &node.text {
        for line in text.lines().take(6) {
            rows.push(format!("│ {line}"));
        }
    }
    rows.into_iter()
        .map(|row| kiqi_tui::utils::truncate_to_width(&row, width, "…"))
        .collect()
}

fn human_ms(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// A bordered box around `rows`, `width` columns wide (including borders).
fn boxed(title: &str, rows: &[String], width: usize) -> Vec<String> {
    let inner = width.saturating_sub(4);
    let title = kiqi_tui::utils::truncate_to_width(title, width.saturating_sub(4), "…");
    let mut out = Vec::new();
    out.push(format!(
        "┌─ {title} {}",
        "─".repeat(width.saturating_sub(title.len() + 4).max(1))
    ));
    for row in rows {
        let row = kiqi_tui::utils::truncate_to_width(row, inner, "…");
        out.push(format!(
            "│ {row}{}│",
            " ".repeat(inner.saturating_sub(row.chars().count()))
        ));
    }
    out.push(format!("└{}┘", "─".repeat(width.saturating_sub(2))));
    out
}

pub(super) struct WorkView {
    pub state: Arc<TuiState>,
}

impl Component for WorkView {
    fn render(&self, width: usize) -> Vec<String> {
        let colors = current_theme().colors;
        let mut lines: Vec<String> = Vec::new();

        let todos = self.state.todo_snapshot();
        if !todos.is_empty() {
            lines.push(colors.accent.fg("tasks"));
            for todo in todos.iter().take(6) {
                lines.push(format!("  {todo}"));
            }
            lines.push(String::new());
        }

        let nodes = self.state.node_snapshot();
        let running = self.state.running_snapshot();
        if nodes.is_empty() && running.is_empty() {
            lines.push(
                colors
                    .muted
                    .fg("no background work right now · /work closes this view"),
            );
        } else {
            let columns = if width >= 140 {
                3
            } else if width >= 80 {
                2
            } else {
                1
            };
            let gap = 2;
            let pane_width = (width.saturating_sub(gap * (columns - 1))) / columns;
            for chunk in nodes.chunks(columns) {
                let mut panes: Vec<Vec<String>> = Vec::new();
                for node in chunk {
                    let title =
                        format!("{} · {} {}", node.id, node.state, human_ms(node.elapsed_ms));
                    panes.push(boxed(
                        &title,
                        &pane_rows(&self.state, node, pane_width),
                        pane_width,
                    ));
                }
                let height = panes.iter().map(Vec::len).max().unwrap_or(0);
                for row in 0..height {
                    let mut line = String::new();
                    for (index, pane) in panes.iter().enumerate() {
                        if index > 0 {
                            line.push_str(&" ".repeat(gap));
                        }
                        line.push_str(pane.get(row).map(String::as_str).unwrap_or(""));
                    }
                    lines.push(line);
                }
                lines.push(String::new());
            }
            if nodes.is_empty() {
                // No subagents: still show what the host sees in flight.
                lines.push(colors.accent.fg("running"));
                for (name, secs, detail, tail) in running {
                    lines.push(format!("  ▶ {name} {secs}s  {detail}"));
                    for line in tail {
                        lines.push(format!("    │ {line}"));
                    }
                }
            }
        }

        let (turns, input, output, _cache) = self.state.llm_usage_snapshot();
        if turns > 0 {
            lines.push(String::new());
            lines.push(colors.muted.fg(&format!(
                "llm {} · turns {turns} · ↑{input} ↓{output}",
                self.state.current_model_id()
            )));
        }
        lines
    }

    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
