//! The work view: what the agents are doing, over the transcript.
//!
//! `/work` overlays it on the output area (the input stays visible); it is on
//! by default and draws nothing while there is no live work. It is not a
//! second transcript: the point is to *watch work* — the todo list, running
//! tools and LLM activity in one summary box, and one draggable box per
//! subagent with its task, state, elapsed time, current tool and recent event
//! stream.

use super::*;

/// One subagent pane's lines, without borders.
fn pane_rows(node: &NodeRow, width: usize) -> Vec<String> {
    let elapsed = human_ms(node.elapsed_ms);
    let budget = if node.budget_ms > 0 {
        human_ms(node.budget_ms)
    } else {
        "—".to_string()
    };
    let mut rows = Vec::new();
    if let Some(task) = &node.task {
        let task: String = task
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect();
        rows.push(format!("task    {task}"));
    }
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
    // The event stream: tool transitions, newest last.
    let shown: Vec<&String> = node.events.iter().rev().take(10).collect();
    for event in shown.into_iter().rev() {
        rows.push(format!("│ {event}"));
    }
    // The child's own words. Shown even when it already ran tools: the raw
    // progress rows no longer appear in the transcript, so this box is the only
    // live view of what the subagent is saying.
    if let Some(text) = &node.text {
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let start = lines.len().saturating_sub(4);
        for line in &lines[start..] {
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
/// Every row is exactly `width` cells so the frame lines up.
fn boxed(title: &str, rows: &[String], width: usize) -> Vec<String> {
    let width = width.max(12);
    let inner = width - 4;
    // Leave one cell for the fill: `truncate_to_width` to `inner` alone would
    // let a full-width title push the top-right corner past the side border.
    let title = kiqi_tui::utils::truncate_to_width(title, inner.saturating_sub(1), "…");
    let title_width = kiqi_tui::ansi::visible_width(&title);
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(format!(
        "┌─ {title} {}┐",
        "─".repeat(width.saturating_sub(title_width + 5))
    ));
    for row in rows {
        let row = kiqi_tui::utils::truncate_to_width(row, inner, "…");
        let pad = inner.saturating_sub(kiqi_tui::ansi::visible_width(&row));
        out.push(format!("│ {row}{} │", " ".repeat(pad)));
    }
    out.push(format!("└{}┘", "─".repeat(width - 2)));
    out.into_iter().map(|row| on_panel(&row)).collect()
}

/// Paint a finished row on the panel surface.
///
/// Terminal cells have no alpha, so a truly see-through overlay turns into two
/// interleaved texts. What reads as "frosted glass" instead is a panel: every
/// cell of the box gets the theme's surface background and its paired
/// foreground, so the transcript behind is replaced by a layer that clearly
/// belongs to the box.
///
/// The background is re-applied after every nested reset: the body lines carry
/// their own `accent`/`muted` spans, and a plain `\x1b[0m` inside one of them
/// would otherwise punch the terminal background back through the row.
fn on_panel(row: &str) -> String {
    let colors = current_theme().colors;
    let background = colors.surface.to_bg();
    let foreground = colors.surface_text.to_fg();
    let nested = row.replace("\x1b[0m", &format!("\x1b[0m{background}{foreground}"));
    format!("{background}{foreground}{nested}\x1b[0m")
}

/// Draw `body` inside a titled box.
///
/// A terminal has no alpha, so "floating" means exactly this: a bounded
/// rectangle that replaces only its own cells. The overlay itself is sized to
/// the box (auto width), not to the screen.
fn frame(body: &[String], width: usize) -> Vec<String> {
    const TITLE: &str = "WORK · /work to close";
    const MAX_WIDTH: usize = 140;
    const MIN_WIDTH: usize = 60;
    let visible = |row: &str| kiqi_tui::ansi::visible_width(row);
    let inner = body
        .iter()
        .map(|row| visible(row))
        .max()
        .unwrap_or(0)
        .max(visible(TITLE) + 1)
        .max(MIN_WIDTH)
        .min(width.saturating_sub(6).min(MAX_WIDTH));
    let mut rows = Vec::with_capacity(body.len() + 2);
    // Widths are display cells, not bytes: the border glyphs are three bytes
    // and the interpunct two, so `str::len()` here left the top border five
    // cells short of the body. Leave a cell of fill so a narrow terminal cannot
    // push the corner past the side border.
    let title = kiqi_tui::utils::truncate_to_width(TITLE, inner.saturating_sub(2), "…");
    let head = format!("┌─ {title} ");
    let fill = (inner + 4).saturating_sub(visible(&head) + 1).max(1);
    rows.push(format!("{head}{}┐", "─".repeat(fill)));
    for row in body {
        let row = kiqi_tui::utils::truncate_to_width(row, inner, "…");
        let pad = inner.saturating_sub(visible(&row));
        rows.push(format!("│ {row}{} │", " ".repeat(pad)));
    }
    rows.push(format!("└{}┘", "─".repeat(inner + 2)));
    rows.into_iter().map(|row| on_panel(&row)).collect()
}

pub(super) struct WorkView {
    pub state: Arc<TuiState>,
    /// Dock rendering (regular mode) hides itself when `/work` is off; the
    /// overlay in fullscreen only exists while it is on.
    pub inline: bool,
}

impl Component for WorkView {
    fn render(&self, width: usize) -> Vec<String> {
        if self.inline && !self.state.work_view_enabled() {
            return Vec::new();
        }
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

        let running = self.state.running_snapshot();
        let streaming = self.state.llm_live_snapshot();
        // Subagents are their own draggable boxes in fullscreen (`NodeBox`);
        // the dock used by `--tui-mode regular` has no compositing layer, so it
        // lists them as one compact line each instead. The box stays out of the
        // way when nothing is happening: a finished turn is not "happening".
        let dock_nodes = if self.inline {
            self.state.node_snapshot()
        } else {
            Vec::new()
        };
        let queued = self.state.queued_agent_calls();
        if running.is_empty()
            && todos.is_empty()
            && streaming.is_none()
            && dock_nodes.is_empty()
            && queued.is_empty()
        {
            return Vec::new();
        }
        if !running.is_empty() {
            lines.push(colors.accent.fg("running"));
            for (name, secs, detail, tail) in running {
                lines.push(format!("  ▶ {name} {secs}s  {detail}"));
                for line in tail {
                    lines.push(format!("    │ {line}"));
                }
            }
        }
        if !queued.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(
                colors
                    .muted
                    .fg("queued · waiting for a free extension call"),
            );
            for (name, secs, detail) in &queued {
                let detail = kiqi_tui::utils::truncate_to_width(detail, 72, "…");
                lines.push(format!("  ⏳ {name} {secs}s  {detail}"));
            }
        }
        if !dock_nodes.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(colors.accent.fg("agents"));
            for node in &dock_nodes {
                let mut line = format!(
                    "  {} · {} {}",
                    node.id,
                    node.state,
                    human_ms(node.elapsed_ms)
                );
                if let Some(task) = node
                    .task
                    .as_deref()
                    .and_then(|task| task.lines().next())
                    .filter(|task| !task.is_empty())
                {
                    line.push_str(&format!(" · {task}"));
                } else if let Some(tool) = node.tool.as_deref() {
                    line.push_str(&format!(" · ▶ {tool}"));
                }
                lines.push(line);
                if let Some(text) = node
                    .text
                    .as_deref()
                    .and_then(|text| text.lines().rev().find(|line| !line.trim().is_empty()))
                {
                    let text = kiqi_tui::utils::truncate_to_width(text.trim(), 72, "…");
                    lines.push(format!("    │ {text}"));
                }
            }
        }
        let (turns, input, output, _cache) = self.state.llm_usage_snapshot();
        if turns > 0 || streaming.is_some() {
            lines.push(String::new());
            if let Some((secs, chars)) = streaming {
                lines.push(
                    colors
                        .accent
                        .fg(&format!("llm streaming {secs}s · {chars} chars")),
                );
            }
            if turns > 0 {
                lines.push(colors.muted.fg(&format!(
                    "llm {} · turns {turns} · ↑{input} ↓{output}",
                    self.state.current_model_id()
                )));
            }
            if let Some((ms, tokens)) = self.state.llm_last_snapshot() {
                lines.push(colors.muted.fg(&format!(
                    "last turn {:.1}s · ↓{tokens} tok",
                    ms as f64 / 1000.0
                )));
            }
        }
        frame(&lines, width)
    }

    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// One subagent, as its own draggable box.
pub(super) struct NodeBox {
    pub state: Arc<TuiState>,
    pub id: String,
}

impl Component for NodeBox {
    fn render(&self, width: usize) -> Vec<String> {
        let Some(node) = self.state.node_snapshot_of(&self.id) else {
            return Vec::new();
        };
        // A pane per subagent, not a full-width band: several agents share the
        // right edge, so the box stays narrow enough to stack. Never widen past
        // the width the overlay manager allocated (a narrow terminal must
        // shrink the box, not have the compositor cut its right border off).
        const NODE_MAX_WIDTH: usize = 68;
        let width = width.min(NODE_MAX_WIDTH);
        let rows = pane_rows(&node, width.saturating_sub(4));
        boxed(
            &format!("{} · {} {}", node.id, node.state, human_ms(node.elapsed_ms)),
            &rows,
            width,
        )
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
