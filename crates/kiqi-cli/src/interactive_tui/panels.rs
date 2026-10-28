//! Generic passive extension panels. No extension names or domain metrics live here.
use super::*;
use kiqi_host::{ExtensionPanel, PanelAnchor, PanelLayout};
use kiqi_tui::ansi::visible_width;
use kiqi_tui::tui::OverlayHandle;
use kiqi_tui::{OverlayAnchor, OverlayOptions};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Session-level LLM usage: turns, tokens and cache reads.
const MODEL_ID: &str = "ki.model";
const MODEL_ORDER: i32 = 7;

pub(super) fn sync_model(
    mailbox: &kiqi_host::ExtensionStatusMailbox,
    model: &str,
    usage: (u64, u64, u64, u64),
    live: Option<(u64, u64)>,
    last: Option<(u64, u64)>,
    shown: &mut bool,
) {
    let (turns, input, output, cache_read) = usage;
    if turns == 0 {
        if *shown {
            let _ = mailbox.component(0, serde_json::json!({ "op": "remove", "id": MODEL_ID }));
            *shown = false;
        }
        return;
    }
    *shown = true;
    let _ = mailbox.component(
        0,
        serde_json::json!({
            "op": "register",
            "id": MODEL_ID,
            "type": "text",
            "slot": "sidebar.right",
            "order": MODEL_ORDER,
            "width": RUNNING_WIDTH,
            "maxHeight": 6,
            "border": true,
            "minScreenWidth": 80,
        }),
    );
    let compact = |value: u64| match value {
        0..=999 => format!("{value}"),
        1_000..=999_999 => format!("{:.1}k", value as f64 / 1_000.0),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    };
    let mut lines = vec![format!("model {model}")];
    if let Some((secs, chars)) = live {
        lines.push(format!("▶ {secs}s  {} ch", compact(chars)));
    } else if let Some((ms, tokens)) = last {
        lines.push(format!("last {:.1}s  {tokens} tok", ms as f64 / 1000.0));
    }
    lines.push(format!("turns {turns}"));
    lines.push(format!("↑{} ↓{}", compact(input), compact(output)));
    lines.push(format!("cache R{}", compact(cache_read)));
    let text = lines.join("\n");
    let _ = mailbox.component(
        0,
        serde_json::json!({ "op": "update", "id": MODEL_ID, "title": "llm", "text": text }),
    );
}

/// The `todo` tool's list, above the extension panels and below `running`.
const TODO_ID: &str = "ki.todo";
const TODO_ORDER: i32 = 5;
const TODO_HEIGHT: usize = 16;

pub(super) fn sync_todo(
    mailbox: &kiqi_host::ExtensionStatusMailbox,
    lines: &[String],
    shown: &mut bool,
) {
    if lines.is_empty() {
        if *shown {
            let _ = mailbox.component(0, serde_json::json!({ "op": "remove", "id": TODO_ID }));
            *shown = false;
        }
        return;
    }
    *shown = true;
    let _ = mailbox.component(
        0,
        serde_json::json!({
            "op": "register",
            "id": TODO_ID,
            "type": "text",
            "slot": "sidebar.right",
            "order": TODO_ORDER,
            "width": RUNNING_WIDTH,
            "maxHeight": TODO_HEIGHT,
            "border": true,
            "minScreenWidth": 80,
        }),
    );
    let _ = mailbox.component(
        0,
        serde_json::json!({
            "op": "update",
            "id": TODO_ID,
            "title": "todo",
            "text": lines.join("\n"),
        }),
    );
}

/// The host's own "running" panel: every tool call in flight, not just the
/// ones an extension publishes. Fed from `TuiState::running_snapshot`, so a
/// long `bash`, an MCP call, a `subagent` or a `dispatch` wave all show up
/// without each extension having to learn the component API.
const RUNNING_ID: &str = "ki.tasks";
const RUNNING_ORDER: i32 = 0;
const RUNNING_WIDTH: usize = 38;
const RUNNING_HEIGHT: usize = 12;

pub(super) fn sync_running(
    mailbox: &kiqi_host::ExtensionStatusMailbox,
    rows: &[(String, u64, String, Vec<String>)],
    shown: &mut bool,
) {
    if rows.is_empty() {
        if *shown {
            let _ = mailbox.component(0, serde_json::json!({ "op": "remove", "id": RUNNING_ID }));
            *shown = false;
        }
        return;
    }
    *shown = true;
    let _ = mailbox.component(
        0,
        serde_json::json!({
            "op": "register",
            "id": RUNNING_ID,
            "type": "text",
            "slot": "sidebar.right",
            "order": RUNNING_ORDER,
            "width": RUNNING_WIDTH,
            "maxHeight": RUNNING_HEIGHT,
            "border": true,
            "minScreenWidth": 80,
        }),
    );
    let text = rows
        .iter()
        .map(|(name, secs, detail, tail)| {
            let elapsed = match secs {
                0..=59 => format!("{secs}s"),
                60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
                _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
            };
            let mut lines = vec![format!("▶ {name}  {elapsed}")];
            if !detail.is_empty() {
                lines.push(format!("  {detail}"));
            }
            for line in tail {
                lines.push(format!("  │ {line}"));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let _ = mailbox.component(
        0,
        serde_json::json!({ "op": "update", "id": RUNNING_ID, "title": "running", "text": text }),
    );
}

struct Panel(Mutex<ExtensionPanel>);
/// Generic extension rows below the built-in usage footer, in both terminal modes.
pub(super) struct FooterPanels(pub kiqi_host::ExtensionStatusMailbox);
impl Component for FooterPanels {
    fn render(&self, width: usize) -> Vec<String> {
        self.0
            .panels()
            .into_values()
            .filter(|p| p.layout == PanelLayout::Footer && width >= p.min_screen_width)
            .flat_map(|spec| Panel(Mutex::new(spec)).render(width))
            .collect()
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
/// Native scrollback has no constrained transcript viewport. Keep sidebar
/// content adjacent to the editor instead of attaching it to historical rows.
pub(super) struct RegularPanels {
    pub mailbox: kiqi_host::ExtensionStatusMailbox,
    pub enabled: bool,
}
impl Component for RegularPanels {
    fn render(&self, width: usize) -> Vec<String> {
        if !self.enabled {
            return vec![];
        }
        let mut rows = Vec::new();
        for spec in self
            .mailbox
            .panels()
            .into_values()
            .filter(|p| p.layout == PanelLayout::Sidebar && width >= p.min_screen_width)
        {
            let left = left_anchor(spec.anchor);
            let panel = Panel(Mutex::new(spec));
            for row in panel.render(width) {
                let padding = " ".repeat(width.saturating_sub(visible_width(&row)));
                rows.push(if left { row } else { format!("{padding}{row}") });
            }
        }
        rows
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn clean(s: &str) -> String {
    kiqi_tui::strip_ansi(s)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}
impl Component for Panel {
    fn render(&self, available_width: usize) -> Vec<String> {
        let spec = self.0.lock().unwrap_or_else(|p| p.into_inner());
        // Visibility is checked against terminal width by Panels, while this
        // component receives only its allocated overlay/sidebar width.
        let width = available_width.min(spec.width);
        if width < 4 {
            return vec![];
        }
        let colors = current_theme().colors;
        let inner = width.saturating_sub(if spec.border { 4 } else { 0 });
        let fit = |text: &str| kiqi_tui::utils::truncate_to_width(&clean(text), inner, "");
        let mut content = Vec::new();
        if !spec.title.is_empty() {
            content.push(fit(&spec.title));
        }
        content.extend(spec.lines.iter().map(|row| fit(row)));
        let mut rows = Vec::new();
        if spec.border && spec.max_height >= 2 {
            rows.push(colors.dim.fg(&format!("┌{}┐", "─".repeat(width - 2))));
            for row in content.into_iter().take(spec.max_height - 2) {
                rows.push(colors.dim.fg(&format!(
                    "│ {row}{} │",
                    " ".repeat(inner.saturating_sub(visible_width(&row)))
                )));
            }
            rows.push(colors.dim.fg(&format!("└{}┘", "─".repeat(width - 2))));
        } else {
            rows.extend(
                content
                    .into_iter()
                    .take(spec.max_height)
                    .map(|row| colors.dim.fg(&row)),
            );
        }
        rows
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn options(spec: &ExtensionPanel, bottom_margin: usize) -> OverlayOptions {
    let anchor = match spec.anchor {
        PanelAnchor::TopLeft => OverlayAnchor::TopLeft,
        PanelAnchor::TopCenter => OverlayAnchor::TopCenter,
        PanelAnchor::TopRight => OverlayAnchor::TopRight,
        PanelAnchor::LeftCenter => OverlayAnchor::LeftCenter,
        PanelAnchor::Center => OverlayAnchor::Center,
        PanelAnchor::RightCenter => OverlayAnchor::RightCenter,
        PanelAnchor::BottomLeft => OverlayAnchor::BottomLeft,
        PanelAnchor::BottomCenter => OverlayAnchor::BottomCenter,
        PanelAnchor::BottomRight => OverlayAnchor::BottomRight,
    };
    OverlayOptions {
        anchor,
        offset_x: spec.offset_x,
        offset_y: spec.offset_y,
        width: Some(kiqi_tui::tui::SizeValue::Absolute(spec.width)),
        max_height: Some(kiqi_tui::tui::SizeValue::Absolute(spec.max_height)),
        non_capturing: true,
        margin: Some(kiqi_tui::tui::OverlayMargin {
            bottom: bottom_margin,
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn left_anchor(anchor: PanelAnchor) -> bool {
    matches!(
        anchor,
        PanelAnchor::TopLeft | PanelAnchor::LeftCenter | PanelAnchor::BottomLeft
    )
}
struct Rail {
    mailbox: kiqi_host::ExtensionStatusMailbox,
    left: bool,
    height: AtomicUsize,
    screen_width: AtomicUsize,
}
impl Component for Rail {
    fn render(&self, width: usize) -> Vec<String> {
        if width < 6 {
            return vec![];
        }
        let screen_width = self.screen_width.load(Ordering::Relaxed);
        let mut specs = self
            .mailbox
            .panels()
            .into_values()
            .filter(|p| {
                p.layout == PanelLayout::Sidebar
                    && left_anchor(p.anchor) == self.left
                    && screen_width >= p.min_screen_width
            })
            .collect::<Vec<_>>();
        if specs.is_empty() {
            return vec![];
        }
        let anchor = specs[0].anchor;
        let offset = specs[0].offset_y;
        let height = self.height.load(Ordering::Relaxed).max(1);
        let single = specs.len() == 1;
        let mut rows = Vec::new();
        for spec in &mut specs {
            spec.min_screen_width = 0;
            if !rows.is_empty() {
                rows.push(String::new());
            }
            let panel = Panel(Mutex::new(spec.clone()));
            for row in panel.render(width - 2) {
                let padding = " ".repeat((width - 2).saturating_sub(visible_width(&row)));
                rows.push(if self.left {
                    format!("{row}{padding} │")
                } else {
                    format!("│ {row}{padding}")
                });
            }
        }
        rows.truncate(height);
        if single {
            let free = height.saturating_sub(rows.len());
            let start = match anchor {
                PanelAnchor::BottomLeft | PanelAnchor::BottomCenter | PanelAnchor::BottomRight => {
                    free
                }
                PanelAnchor::LeftCenter | PanelAnchor::Center | PanelAnchor::RightCenter => {
                    free / 2
                }
                _ => 0,
            };
            let start = (start as i64 + offset as i64).clamp(0, free as i64) as usize;
            rows.splice(0..0, std::iter::repeat_n(String::new(), start));
        }
        rows
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Dock {
    stack: Arc<kiqi_tui::HStack>,
    scroll: Arc<ScrollView>,
    left: Arc<Rail>,
    right: Arc<Rail>,
    sizes: Option<(usize, usize)>,
}
#[derive(Default)]
pub(super) struct Panels {
    pub regular_mode: bool,
    entries: BTreeMap<String, (Arc<Panel>, Arc<dyn OverlayHandle>)>,
    dock: Option<Dock>,
    revision: Option<u64>,
    viewport: (usize, usize),
    screen_height: usize,
}
impl Panels {
    pub(super) fn with_sidebar(
        stack: Arc<kiqi_tui::HStack>,
        scroll: Arc<ScrollView>,
        mailbox: kiqi_host::ExtensionStatusMailbox,
    ) -> Self {
        let rail = |left| {
            Arc::new(Rail {
                mailbox: mailbox.clone(),
                left,
                height: AtomicUsize::new(1),
                screen_width: AtomicUsize::new(0),
            })
        };
        let mut panels = Self::default();
        panels.dock = Some(Dock {
            stack,
            scroll,
            left: rail(true),
            right: rail(false),
            sizes: None,
        });
        panels
    }
    pub(super) fn sync(
        &mut self,
        tui: &TuiAltScreen,
        mailbox: &kiqi_host::ExtensionStatusMailbox,
    ) -> bool {
        let viewport = (
            tui.width(),
            self.dock
                .as_ref()
                .map(|dock| dock.scroll.viewport_height())
                .unwrap_or(0),
        );
        self.sync_viewport(tui, mailbox, viewport)
    }
    fn sync_viewport(
        &mut self,
        tui: &dyn TUI,
        mailbox: &kiqi_host::ExtensionStatusMailbox,
        viewport: (usize, usize),
    ) -> bool {
        let revision = mailbox.revision();
        let screen_height = tui.terminal().rows();
        let viewport_changed = self.viewport != viewport || self.screen_height != screen_height;
        if self.revision == Some(revision) && !viewport_changed {
            return false;
        }
        self.revision = Some(revision);
        self.viewport = viewport;
        self.screen_height = screen_height;
        let mut specs = mailbox.panels();
        if let Some(dock) = &mut self.dock {
            let (mut left, mut right) = (0, 0);
            for spec in specs.values().filter(|spec| {
                spec.layout == PanelLayout::Sidebar && viewport.0 >= spec.min_screen_width
            }) {
                if left_anchor(spec.anchor) {
                    left = left.max(spec.width + 2);
                } else {
                    right = right.max(spec.width + 2);
                }
            }
            // Keep at least 48 columns for chat. Narrow screens use the original full-width layout.
            if self.regular_mode || viewport.0.saturating_sub(left + right) < 48 {
                left = 0;
                right = 0;
            }
            dock.left.height.store(viewport.1, Ordering::Relaxed);
            dock.right.height.store(viewport.1, Ordering::Relaxed);
            dock.left.screen_width.store(viewport.0, Ordering::Relaxed);
            dock.right.screen_width.store(viewport.0, Ordering::Relaxed);
            if dock.sizes != Some((left, right)) {
                dock.stack.clear();
                if left > 0 {
                    dock.stack.add_child_with_options(
                        dock.left.clone(),
                        kiqi_tui::StackEntryOptions {
                            basis: Some(left),
                            shrink: 0,
                            ..Default::default()
                        },
                    );
                }
                dock.stack.add_child_with_options(
                    dock.scroll.clone(),
                    kiqi_tui::StackEntryOptions {
                        basis: Some(0),
                        grow: 1,
                        shrink: 1,
                        min_size: 1,
                        ..Default::default()
                    },
                );
                if right > 0 {
                    dock.stack.add_child_with_options(
                        dock.right.clone(),
                        kiqi_tui::StackEntryOptions {
                            basis: Some(right),
                            shrink: 0,
                            ..Default::default()
                        },
                    );
                }
                dock.sizes = Some((left, right));
            }
        }
        specs.retain(|_, spec| {
            spec.layout == PanelLayout::Overlay && viewport.0 >= spec.min_screen_width
        });
        // Passive extension panels belong to the transcript. Leave the editor,
        // footer and active input dialogs visible even for bottom anchors.
        let bottom_margin = if self.dock.is_some() {
            screen_height.saturating_sub(viewport.1)
        } else {
            0
        };
        self.entries.retain(|key, (_, handle)| {
            if specs.contains_key(key) {
                true
            } else {
                handle.hide();
                false
            }
        });
        for (key, spec) in specs {
            if let Some((panel, handle)) = self.entries.get(&key) {
                let mut current = panel.0.lock().unwrap_or_else(|p| p.into_inner());
                if current.same_geometry(&spec) && !viewport_changed {
                    *current = spec;
                    continue;
                }
                handle.hide();
            }
            let opts = options(&spec, bottom_margin);
            let panel = Arc::new(Panel(Mutex::new(spec)));
            let handle = tui.show_overlay(panel.clone(), Some(opts));
            self.entries.insert(key, (panel, handle));
        }
        true
    }
}
impl Drop for Panels {
    fn drop(&mut self) {
        for (_, handle) in self.entries.values() {
            handle.hide();
        }
    }
}

#[cfg(test)]
mod tests {
    fn set_component(
        m: &kiqi_host::ExtensionStatusMailbox,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = args["key"].as_str().unwrap();
        let p = &args["panel"];
        if p.is_null() {
            return m.component(0, serde_json::json!({"op":"remove","id":id}));
        }
        m.component(0, serde_json::json!({"op":"register","id":id}))?;
        let mut config = p.clone();
        let obj = config.as_object_mut().unwrap();
        let layout = obj
            .remove("layout")
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or("overlay".into());
        obj.remove("version");
        obj.remove("lines");
        obj.remove("title");
        obj.insert(
            "slot".into(),
            serde_json::json!(match layout.as_str() {
                "footer" => "footer.after",
                "sidebar" => "sidebar.right",
                _ => "overlay",
            }),
        );
        obj.insert("op".into(), serde_json::json!("configure"));
        obj.insert("id".into(), serde_json::json!(id));
        m.component(0, config)?;
        let text = p["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        m.component(0,serde_json::json!({"op":"update","id":id,"text":text,"title":p["title"].as_str().unwrap_or("")}))
    }

    use super::*;
    #[test]
    fn model_and_todo_panels_publish_and_remove_themselves() {
        let mailbox = kiqi_host::ExtensionStatusMailbox::new();
        let mut model_shown = false;
        sync_model(
            &mailbox,
            "deepseek/flash",
            (2, 1_200, 345, 8_000),
            None,
            Some((2_100, 345)),
            &mut model_shown,
        );
        let llm = mailbox
            .panels()
            .into_values()
            .find(|panel| panel.title == "llm")
            .expect("llm panel published");
        assert!(
            llm.lines.iter().any(|line| line.contains("↑1.2k")),
            "{:?}",
            llm.lines
        );
        assert!(llm.lines.iter().any(|line| line.contains("deepseek/flash")));

        let mut todo_shown = false;
        sync_todo(&mailbox, &["✔ ship it".to_string()], &mut todo_shown);
        assert!(mailbox.panels().values().any(|panel| panel.title == "todo"));
        sync_todo(&mailbox, &[], &mut todo_shown);
        assert!(
            !mailbox.panels().values().any(|panel| panel.title == "todo"),
            "an empty list removes the panel"
        );
    }

    #[test]
    fn footer_monitor_follows_usage_row_and_hides_without_taking_sidebar_space() {
        let mailbox = kiqi_host::ExtensionStatusMailbox::new();
        let monitor = Arc::new(FooterPanels(mailbox.clone()));
        assert!(monitor.render(120).is_empty());
        set_component(&mailbox,serde_json::json!({"key":"monitor","panel":{"version":1,"layout":"footer","width":240,"maxHeight":1,"border":false,"lines":["Turns 1 · Steps 87 · TPS 252 · TTFT 0.42s · Latency 2.1s · TPM 15.1k"]}})).unwrap();
        let root = VStack::from_children(vec![
            StackChild::Entry(StackEntry::new(Arc::new(Text::new(
                "↑315k ↓315k R86M CH99.7%",
                0,
                0,
            )))),
            StackChild::Entry(StackEntry::new(monitor.clone())),
        ]);
        let rows = root.render(120);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("↑315k"));
        assert!(rows[1].contains("TPS 252"));
        assert_eq!(monitor.render(40).len(), 1);
        assert!(monitor
            .render(40)
            .iter()
            .all(|row| visible_width(row) <= 40));
        set_component(&mailbox, serde_json::json!({"key":"monitor","panel":null})).unwrap();
        assert!(monitor.render(120).is_empty());
    }
    #[test]
    fn regular_mode_keeps_monitor_next_to_editor_without_reserving_chat_columns() {
        let mailbox = kiqi_host::ExtensionStatusMailbox::new();
        set_component(&mailbox,serde_json::json!({"key":"monitor","panel":{"version":1,"layout":"sidebar","width":24,"maxHeight":11,"minScreenWidth":80,"border":false,"title":"RUN STATS","lines":["TTFT 0.42 s","TPS 252 tok/s","Cost USD 0.0123"]}})).unwrap();
        let fallback = Arc::new(RegularPanels {
            mailbox: mailbox.clone(),
            enabled: true,
        });
        assert_eq!(fallback.render(120).len(), 4);
        assert!(fallback.render(120)[1].contains("TTFT"));
        assert!(fallback.render(79).is_empty());
        assert!(RegularPanels {
            mailbox: mailbox.clone(),
            enabled: false
        }
        .render(120)
        .is_empty());
        let root = VStack::from_children(vec![
            StackChild::Entry(StackEntry::new(Arc::new(Text::new(
                "history\n".repeat(100),
                0,
                0,
            )))),
            StackChild::Entry(StackEntry::new(fallback)),
            StackChild::Entry(StackEntry::new(Arc::new(Text::new("INPUT", 0, 0)))),
        ]);
        let rows = root.render(120);
        assert!(rows.iter().rev().take(5).any(|row| row.contains("TTFT")));
        let tui = TuiAltScreen::new(Box::new(ProcessTerminal::new()), false, None);
        let scroll = Arc::new(ScrollView::new(
            Arc::new(Text::new("chat", 0, 0)),
            ScrollViewOptions::default(),
        ));
        let stack = Arc::new(kiqi_tui::HStack::new());
        let mut panels = Panels::with_sidebar(stack.clone(), scroll, mailbox.clone());
        panels.regular_mode = true;
        panels.sync_viewport(&tui, &mailbox, (120, 0));
        assert_eq!(stack.get_children().len(), 1);
    }
    #[test]
    fn overlay_visibility_uses_terminal_width_and_reflows_with_resize() {
        let tui = TuiAltScreen::new(Box::new(ProcessTerminal::new()), false, None);
        let mailbox = kiqi_host::ExtensionStatusMailbox::new();
        let mut panels = Panels::default();
        set_component(&mailbox,serde_json::json!({"key":"monitor","panel":{"version":1,"width":24,"minScreenWidth":80,"lines":["visible"]}})).unwrap();
        panels.sync_viewport(&tui, &mailbox, (120, 21));
        let panel = panels.entries["component:2147483648:0:monitor"].0.clone();
        assert!(panel.render(24).join("\n").contains("visible"));
        panels.sync_viewport(&tui, &mailbox, (64, 21));
        assert!(panels.entries.is_empty());
        panels.sync_viewport(&tui, &mailbox, (120, 21));
        assert!(panels.entries["component:2147483648:0:monitor"]
            .0
            .render(24)
            .join("\n")
            .contains("visible"));
        assert!(!Arc::ptr_eq(
            &panel,
            &panels.entries["component:2147483648:0:monitor"].0
        ));
    }
    #[test]
    fn sidebar_reserves_space_preserves_scroll_and_editor_and_restores_layout_when_hidden() {
        let tui = TuiAltScreen::new(Box::new(ProcessTerminal::new()), false, None);
        let mailbox = kiqi_host::ExtensionStatusMailbox::new();
        let scroll = Arc::new(ScrollView::new(
            Arc::new(Text::new("CHAT\n".repeat(50), 0, 0)),
            ScrollViewOptions {
                primary: true,
                follow: FollowMode::End,
                scrollbar: ScrollbarMode::Hidden,
                ..Default::default()
            },
        ));
        let stack = Arc::new(kiqi_tui::HStack::from_entries(vec![StackEntry::new(
            scroll.clone(),
        )
        .basis(0)
        .grow(1)]));
        let root: Arc<dyn Component> = Arc::new(VStack::from_children(vec![
            StackChild::Entry(StackEntry::new(stack.clone()).basis(0).grow(1).min_size(1)),
            StackChild::Entry(
                StackEntry::new(Arc::new(Text::new("INPUT", 0, 0)))
                    .basis(3)
                    .shrink(0),
            ),
        ]));
        let mut panels = Panels::with_sidebar(stack, scroll.clone(), mailbox.clone());
        panels.sync_viewport(&tui, &mailbox, (120, 21));
        let frame = kiqi_tui::layout::render_layout_frame(root.clone(), 120, 24);
        assert_eq!(frame.root.children[0].children[0].rect.width, 120);
        set_component(&mailbox,serde_json::json!({"key":"monitor","panel":{"version":1,"layout":"sidebar","width":24,"minScreenWidth":80,"border":false,"lines":["TTFT 0.42 s","TPS 252 tok/s"]}})).unwrap();
        panels.sync_viewport(&tui, &mailbox, (120, 21));
        let frame = kiqi_tui::layout::render_layout_frame(root.clone(), 120, 24);
        assert_eq!(frame.primary_scroll_view.unwrap().viewport_height(), 21);
        assert_eq!(scroll.viewport_height(), 21);
        let chat = &frame.root.children[0].children[0];
        let rail = &frame.root.children[0].children[1];
        assert_eq!(chat.rect.width, 94);
        assert_eq!(chat.rect.height, 21);
        assert_eq!(rail.rect.x, 94);
        assert_eq!(rail.rect.width, 26);
        assert_eq!(frame.root.children[1].rect.width, 120);
        assert!(frame.lines[0].contains("TTFT 0.42 s"));
        assert!(frame.lines[21].contains("INPUT"));
        assert!(!tui.has_overlay());
        panels.sync_viewport(&tui, &mailbox, (64, 21));
        let frame = kiqi_tui::layout::render_layout_frame(root.clone(), 64, 24);
        assert_eq!(frame.root.children[0].children.len(), 1);
        assert_eq!(frame.root.children[0].children[0].rect.width, 64);
        set_component(&mailbox, serde_json::json!({"key":"monitor","panel":null})).unwrap();
        panels.sync_viewport(&tui, &mailbox, (120, 21));
        let frame = kiqi_tui::layout::render_layout_frame(root, 120, 24);
        assert_eq!(frame.root.children[0].children[0].rect.width, 120);
    }
    #[test]
    fn multiple_panels_update_move_and_remove_without_stealing_editor_focus() {
        // Do not start the terminal: registry/focus operations need no IO.
        let tui = TuiAltScreen::new(Box::new(ProcessTerminal::new()), false, None);
        let editor: Arc<dyn Component> = Arc::new(Editor::simple());
        tui.set_focus(Some(editor.clone()));
        let mailbox = kiqi_host::ExtensionStatusMailbox::new();
        let mut panels = Panels::default();
        set_component(&mailbox,serde_json::json!({"key":"a","panel":{"version":1,"anchor":"top-left","lines":["first"]}})).unwrap();
        set_component(&mailbox,serde_json::json!({"key":"b","panel":{"version":1,"anchor":"bottom-right","lines":["other"]}})).unwrap();
        panels.sync(&tui, &mailbox);
        assert_eq!(panels.entries.len(), 2);
        assert!(Arc::ptr_eq(&tui.get_focus().unwrap(), &editor));
        let original = panels.entries["component:2147483648:0:a"].0.clone();
        set_component(&mailbox,serde_json::json!({"key":"a","panel":{"version":1,"anchor":"top-left","lines":["updated"]}})).unwrap();
        panels.sync(&tui, &mailbox);
        assert!(Arc::ptr_eq(
            &original,
            &panels.entries["component:2147483648:0:a"].0
        ));
        assert!(original.render(80).join("\n").contains("updated"));
        set_component(&mailbox,serde_json::json!({"key":"a","panel":{"version":1,"anchor":"bottom-left","offsetX":2,"lines":["moved"]}})).unwrap();
        panels.sync(&tui, &mailbox);
        assert!(!Arc::ptr_eq(
            &original,
            &panels.entries["component:2147483648:0:a"].0
        ));
        assert_eq!(
            panels.entries["component:2147483648:0:a"]
                .0
                 .0
                .lock()
                .unwrap()
                .anchor,
            PanelAnchor::BottomLeft
        );
        set_component(&mailbox, serde_json::json!({"key":"a","panel":null})).unwrap();
        panels.sync(&tui, &mailbox);
        assert_eq!(panels.entries.len(), 1);
        assert!(tui.has_overlay());
        drop(panels);
        assert!(!tui.has_overlay());
        assert!(Arc::ptr_eq(&tui.get_focus().unwrap(), &editor));
    }
    #[test]
    fn arbitrary_extension_content_is_sanitized_and_fits_unicode_and_height_limits() {
        let spec:ExtensionPanel=serde_json::from_value(serde_json::json!({"version":1,"width":44,"maxHeight":4,
            "title":"插件\u{1b}[31m标题\u{1b}[0m","lines":["1 轮 87 步 252 tok/s","中文宽度测试".repeat(30),"truncated"]})).unwrap();
        let panel = Panel(Mutex::new(spec));
        for width in [1, 4, 12, 30, 44, 80] {
            let rows = panel.render(width);
            assert!(rows.len() <= 4);
            assert!(rows.iter().all(|row| visible_width(row) <= width));
            assert!(!rows.join("\n").contains("truncated"));
        }
        assert!(panel.render(80).join("\n").contains("1 轮 87 步 252 tok/s"));
    }
}
