//! The top status panel: brand, where you are, and what you are running.
//!
//! Three fixed rows (the root VStack gives the panel `.shrink(0)`, and the
//! allocator never shrinks a `shrink(0)` entry), so neither a tall dock nor a
//! long transcript can take a row from it:
//!
//! ```text
//!  ▃ ▅ ▂  kiqi  v0.5.5                          claude-sonnet-5-5
//!  ~/GitHub/pi-rust
//!  main · 9 modules
//! ```
//!
//! The footer keeps what is happening *right now* (usage, hints); this panel
//! carries the stable context.

use super::*;

/// The panel is exactly this tall; `render` always returns this many rows.
pub(super) const HEADER_ROWS: usize = 3;

pub(super) struct HeaderComponent {
    cwd: std::sync::Mutex<Option<String>>,
    home: std::sync::Mutex<Option<String>>,
    branch: std::sync::Mutex<Option<String>>,
    model: std::sync::Mutex<String>,
    modules: std::sync::Mutex<usize>,
}

impl HeaderComponent {
    pub(super) fn new() -> Self {
        Self {
            cwd: std::sync::Mutex::new(None),
            home: std::sync::Mutex::new(
                std::env::var("HOME")
                    .ok()
                    .or_else(|| std::env::var("USERPROFILE").ok()),
            ),
            branch: std::sync::Mutex::new(None),
            model: std::sync::Mutex::new(String::new()),
            modules: std::sync::Mutex::new(0),
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

    /// The active model id, shown on the first row.
    pub(super) fn set_model(&self, model: &str) {
        if let Ok(mut cell) = self.model.lock() {
            *cell = model.to_string();
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
        let model = self
            .model
            .lock()
            .map(|cell| cell.clone())
            .unwrap_or_default();
        let modules = self.modules.lock().map(|cell| *cell).unwrap_or(0);

        // Row 1: brand left, model right.
        let brand = format!(
            " {}  {}  {}",
            tui_bold(&colors.accent.fg(crate::brand::MARK)),
            tui_bold(&colors.muted.fg("kiqi")),
            colors.dim.fg(&format!("v{}", crate::VERSION))
        );
        let mut rows = vec![compose(brand, colors.dim.fg(&model), width)];

        // Row 2: the working directory, unclipped to the session root.
        rows.push(truncate_row(
            format!(
                " {}",
                colors
                    .dim
                    .fg(&kiqi_tui::ansi::sanitize_external_text(&self.location()))
            ),
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
        let header = HeaderComponent::new();
        header.set_cwd(cwd);
        header.set_git_branch(branch);
        header.set_model("claude-sonnet-5-5");
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
        assert_eq!(HeaderComponent::new().render(80).len(), HEADER_ROWS);
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
        assert!(rows[0].contains("claude-sonnet-5-5"), "{rows:?}");
        assert!(rows[1].contains("/work/project"), "{rows:?}");
        assert!(rows[2].contains("main"), "{rows:?}");
        assert!(rows[2].contains("9 modules"), "{rows:?}");
    }

    #[test]
    fn home_is_rendered_as_a_tilde() {
        let header = HeaderComponent::new();
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
}
