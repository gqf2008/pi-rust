//! The top status line: brand mark + project location.
//!
//! The footer says what is happening (model, usage, hints); this line says where
//! you are. It is exactly one row and the root VStack gives it `.shrink(0)`, so
//! neither a tall dock nor a long transcript can push it off screen — the top
//! region is structural, not incidental.

use super::*;

/// The top bar: `▃ ▅ ▂ kiqi  ~/project (branch)`.
pub(super) struct HeaderComponent {
    cwd: std::sync::Mutex<Option<String>>,
    home: std::sync::Mutex<Option<String>>,
    branch: std::sync::Mutex<Option<String>>,
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
        }
    }

    /// The session working directory. Rendered `~`-relative when it lives under
    /// the home directory.
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

    fn location(&self) -> String {
        let cwd = self
            .cwd
            .lock()
            .ok()
            .and_then(|cell| cell.clone())
            .unwrap_or_default();
        let home = self.home.lock().ok().and_then(|cell| cell.clone());
        let location = short_path(&cwd, home.as_deref());
        match self.branch.lock().ok().and_then(|cell| cell.clone()) {
            Some(branch) if !branch.is_empty() => format!("{location} ({branch})"),
            _ => location,
        }
    }
}

impl Component for HeaderComponent {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let colors = current_theme().colors;
        // The mark is `▃ ▅ ▂` — block elements are single-width everywhere,
        // unlike the `π` the brand replaced (see `brand.rs`).
        let mark = tui_bold(&colors.accent.fg(crate::brand::MARK));
        let name = tui_bold(&colors.muted.fg("kiqi"));
        let location = kiqi_tui::ansi::sanitize_external_text(&self.location());
        let mut line = format!(" {mark}  {name}");
        if !location.is_empty() {
            line.push_str(&format!("  {}", colors.dim.fg(&location)));
        }
        if kiqi_tui::ansi::visible_width(&line) > width {
            line = kiqi_tui::utils::truncate_to_width(&line, width, "…");
        }
        vec![line]
    }

    fn invalidate(&self) {}

    fn as_any(&self) -> &dyn std::any::Any {
        self
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
        header
    }

    #[test]
    fn renders_the_mark_the_name_and_the_location() {
        let header = header("/work/project", Some("main"));
        let rows = header.render(80);
        assert_eq!(rows.len(), 1, "the top bar is exactly one row");
        let text = kiqi_tui::ansi::strip_ansi(&rows[0]);
        assert!(text.contains(crate::brand::MARK), "{text}");
        assert!(text.contains("kiqi"), "{text}");
        assert!(text.contains("/work/project"), "{text}");
        assert!(text.contains("(main)"), "{text}");
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
            assert_eq!(rows.len(), 1);
            assert!(
                kiqi_tui::ansi::visible_width(&rows[0]) <= width,
                "width {width}: {:?}",
                kiqi_tui::ansi::strip_ansi(&rows[0])
            );
        }
    }
}
