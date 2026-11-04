//! The top status panel: brand, where you are, and what you are running.
//!
//! Three fixed rows (the root VStack gives the panel `.shrink(0)`, and the
//! allocator never shrinks a `shrink(0)` entry), so neither a tall dock nor a
//! long transcript can take a row from it:
//!
//! ```text
//!  ▃ ▅ ▂  kiqi  v0.5.6                          claude-sonnet-5-5
//!  ~/GitHub/pi-rust
//!  main · 9 modules
//! ```
//!
//! The footer keeps what is happening *right now* (usage, hints); this panel
//! carries the stable context.

use super::*;

use std::sync::Arc;

/// The panel is exactly this tall; `render` always returns this many rows.
pub(super) const HEADER_ROWS: usize = 3;

pub(super) struct HeaderComponent {
    cwd: std::sync::Mutex<Option<String>>,
    home: std::sync::Mutex<Option<String>>,
    branch: std::sync::Mutex<Option<String>>,
    modules: std::sync::Mutex<usize>,
    /// The footer owns the context counters; the panel reads them from there
    /// rather than keeping a second copy that could drift.
    footer: Arc<FooterComponent>,
}

impl HeaderComponent {
    pub(super) fn new(footer: Arc<FooterComponent>) -> Self {
        Self {
            cwd: std::sync::Mutex::new(None),
            home: std::sync::Mutex::new(
                std::env::var("HOME")
                    .ok()
                    .or_else(|| std::env::var("USERPROFILE").ok()),
            ),
            branch: std::sync::Mutex::new(None),
            modules: std::sync::Mutex::new(0),
            footer,
        }
    }

    /// The session working directory, rendered `~`-relative under the home dir.
    pub(super) fn set_cwd(&self, cwd: &str) {
        if let Ok(mut cell) = self.cwd.lock() {
            *cell = Some(cwd.to_string());
        }
    }

    /// The current git branch, or `None` outside a repository.
    pub(super) fn set_git_branch(&self, branch: Option<&str>) {
        if let Ok(mut cell) = self.branch.lock() {
            *cell = branch.map(str::to_string);
        }
    }

    /// How many wasm modules are installed, shown on the last row.
    pub(super) fn set_module_count(&self, count: usize) {
        if let Ok(mut cell) = self.modules.lock() {
            *cell = count;
        }
    }

    fn location(&self) -> String {
        let cwd = self
            .cwd
            .lock()
            .ok()
            .and_then(|cell| cell.clone())
            .unwrap_or_default();
        let home = self.home.lock().ok().and_then(|cell| cell.clone());
        short_path(&cwd, home.as_deref())
    }
}

impl Component for HeaderComponent {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let colors = current_theme().colors;
        let model = model_badge(&self.footer, &colors);
        let modules = self.modules.lock().map(|cell| *cell).unwrap_or(0);

        // Row 1: brand left, model right.
        let brand = format!(
            " {}  {}  {}",
            tui_bold(&colors.accent.fg(crate::brand::MARK)),
            tui_bold(&colors.muted.fg("kiqi")),
            colors.dim.fg(&format!("v{}", crate::VERSION))
        );
        let mut rows = vec![compose(brand, model, width)];

        // Row 2: working directory left, context usage right.
        rows.push(compose(
            format!(
                " {}",
                colors
                    .dim
                    .fg(&kiqi_tui::ansi::sanitize_external_text(&self.location()))
            ),
            context_badge(&self.footer, &colors),
            width,
        ));

        // Row 3: branch + installed modules.
        let branch = self
            .branch
            .lock()
            .ok()
            .and_then(|cell| cell.clone())
            .filter(|branch| !branch.is_empty());
        let mut details = Vec::new();
        if let Some(branch) = branch {
            details.push(
                colors
                    .accent
                    .fg(&kiqi_tui::ansi::sanitize_external_text(&branch)),
            );
        }
        details.push(colors.dim.fg(&format!(
            "{modules} module{}",
            if modules == 1 { "" } else { "s" }
        )));
        rows.push(truncate_row(format!(" {}", details.join(" · ")), width));

        // Exactly HEADER_ROWS rows, even when a row is empty: the panel's height
        // must not change with the data, or the transcript would jump.
        rows.resize(HEADER_ROWS, String::new());
        rows
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Right-align `right` on the same row as `left`, when both fit with padding.
fn compose(left: String, right: String, width: usize) -> String {
    let left_w = kiqi_tui::ansi::visible_width(&left);
    let right_w = kiqi_tui::ansi::visible_width(&right);
    if right_w > 0 && left_w + right_w + 2 <= width {
        let pad = " ".repeat(width - left_w - right_w);
        format!("{left}{pad}{right}")
    } else {
        truncate_row(left, width)
    }
}

/// Clip one row to the terminal width (never wider than `width` cells).
fn truncate_row(row: String, width: usize) -> String {
    if kiqi_tui::ansi::visible_width(&row) > width {
        kiqi_tui::utils::truncate_to_width(&row, width, "…")
    } else {
        row
    }
}

/// `████░░░░ 42% · 84K/200K`, coloured by pressure.
///
/// `deepseek/deepseek-flash • medium`.
///
/// The footer owns both halves (`(provider) model` + the thinking level), so a
/// `/model` switch or a thinking-level change reaches this row without a second
/// copy to keep in sync. The footer's parenthesised provider is rewritten to
/// the `provider/model` spelling the panel uses.
fn model_badge(footer: &FooterComponent, colors: &kiqi_tui::theme::ThemeColors) -> String {
    let raw = footer.model_label();
    if raw.is_empty() {
        return String::new();
    }
    let model = match raw.strip_prefix('(').and_then(|rest| rest.split_once(") ")) {
        Some((provider, model)) => format!("{provider}/{model}"),
        None => raw,
    };
    let text = match footer.thinking_level() {
        Some(level) if !level.is_empty() => format!("{model} • {level}"),
        _ => model,
    };
    colors.dim.fg(&text)
}

/// The bar and the numbers come from the footer's counters, so a model switch
/// rescales the denominator and a compaction resets the numerator exactly as
/// the footer's own badge does.
fn context_badge(footer: &FooterComponent, colors: &kiqi_tui::theme::ThemeColors) -> String {
    const CELLS: usize = 8;
    let (percent, tokens, window) = footer.context_badge();
    let Some(percent) = percent else {
        // No percentage without a window: models configured without
        // `contextWindow` (or a session that has not answered yet) still have a
        // token count worth showing. With nothing at all, draw nothing rather
        // than a placeholder that reads like a broken bar.
        return match tokens {
            Some(tokens) if tokens > 0 => colors
                .dim
                .fg(&format!("ctx {} tok", compact_tokens(tokens))),
            _ => String::new(),
        };
    };
    let filled = (((percent / 100.0) * CELLS as f64).round() as usize).min(CELLS);
    let bar: String = (0..CELLS)
        .map(|cell| if cell < filled { '█' } else { '░' })
        .collect();
    let color = if percent >= 90.0 {
        colors.error
    } else if percent >= 70.0 {
        colors.warning
    } else {
        colors.accent
    };
    let mut text = format!("{} {:>3.0}%", color.fg(&bar), percent);
    if let (Some(tokens), Some(window)) = (tokens, window) {
        text.push_str(&format!(
            " · {}/{}",
            compact_tokens(tokens),
            compact_tokens(window as i64)
        ));
    }
    text
}

/// `84_000` → `84K`, `1_200_000` → `1.2M`.
fn compact_tokens(tokens: i64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// `/Users/me/project` → `~/project`; anything else is left alone.
fn short_path(cwd: &str, home: Option<&str>) -> String {
    if cwd.is_empty() {
        return String::new();
    }
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        if let Some(rest) = cwd.strip_prefix(home) {
            if rest.is_empty() {
                return "~".to_string();
            }
            if rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    cwd.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(cwd: &str, branch: Option<&str>) -> HeaderComponent {
        let header = HeaderComponent::new(Arc::new(FooterComponent::new()));
        header.set_cwd(cwd);
        header.set_git_branch(branch);
        header.footer.set_model("(deepseek) deepseek-flash");
        header.footer.set_thinking_level(Some("medium"));
        header.set_module_count(9);
        header
    }

    fn plain(rows: &[String]) -> Vec<String> {
        rows.iter()
            .map(|row| kiqi_tui::ansi::strip_ansi(row))
            .collect()
    }

    #[test]
    fn renders_exactly_three_rows() {
        let header = header("/work/project", Some("main"));
        assert_eq!(header.render(80).len(), HEADER_ROWS);
        // Missing data must not collapse the panel.
        assert_eq!(
            HeaderComponent::new(Arc::new(FooterComponent::new()))
                .render(80)
                .len(),
            HEADER_ROWS
        );
    }

    #[test]
    fn carries_brand_location_branch_model_and_modules() {
        let rows = plain(&header("/work/project", Some("main")).render(80));
        assert!(rows[0].contains(crate::brand::MARK), "{rows:?}");
        assert!(rows[0].contains("kiqi"), "{rows:?}");
        assert!(
            rows[0].contains(&format!("v{}", crate::VERSION)),
            "{rows:?}"
        );
        assert!(
            rows[0].contains("deepseek/deepseek-flash • medium"),
            "{rows:?}"
        );
        assert!(rows[1].contains("/work/project"), "{rows:?}");
        assert!(rows[2].contains("main"), "{rows:?}");
        assert!(rows[2].contains("9 modules"), "{rows:?}");
    }

    #[test]
    fn home_is_rendered_as_a_tilde() {
        let header = HeaderComponent::new(Arc::new(FooterComponent::new()));
        *header.home.lock().unwrap() = Some("/home/me".to_string());
        header.set_cwd("/home/me/project");
        assert_eq!(header.location(), "~/project");
        header.set_cwd("/home/me");
        assert_eq!(header.location(), "~");
    }

    #[test]
    fn never_wider_than_the_terminal() {
        let header = header(
            "/a/very/long/path/that/keeps/going/and/going",
            Some("feature/x"),
        );
        for width in [1, 2, 5, 20, 80] {
            let rows = header.render(width);
            assert_eq!(rows.len(), HEADER_ROWS);
            for row in &rows {
                assert!(
                    kiqi_tui::ansi::visible_width(row) <= width,
                    "width {width}: {:?}",
                    kiqi_tui::ansi::strip_ansi(row)
                );
            }
        }
    }

    #[test]
    fn row_two_carries_the_context_badge() {
        let footer = Arc::new(FooterComponent::new());
        footer.set_context_window(Some(200_000));
        footer.set_context_tokens(Some(84_000));
        let header = HeaderComponent::new(footer);
        header.set_cwd("/work/project");
        let rows = plain(&header.render(80));
        assert!(rows[1].contains("42%"), "{rows:?}");
        assert!(rows[1].contains("84K/200K"), "{rows:?}");
        assert!(rows[1].contains('█'), "{rows:?}");
        assert!(rows[1].contains('░'), "{rows:?}");
    }

    #[test]
    fn unknown_context_renders_nothing_or_the_token_count() {
        // Nothing known yet: no placeholder on the row.
        let header = HeaderComponent::new(Arc::new(FooterComponent::new()));
        header.set_cwd("/work/project");
        let rows = plain(&header.render(80));
        assert!(!rows[1].contains("context"), "{rows:?}");
        assert!(!rows[1].contains('%'), "{rows:?}");

        // Tokens without a window (a model configured without `contextWindow`):
        // show the count, not a percentage that cannot be computed.
        let footer = Arc::new(FooterComponent::new());
        footer.set_context_tokens(Some(84_000));
        let header = HeaderComponent::new(footer);
        header.set_cwd("/work/project");
        let rows = plain(&header.render(80));
        assert!(rows[1].contains("ctx 84K tok"), "{rows:?}");
        assert!(!rows[1].contains('%'), "{rows:?}");
    }
}
