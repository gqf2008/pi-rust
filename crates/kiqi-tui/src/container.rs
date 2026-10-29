//! Container component that holds multiple child components.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;

/// Container - a component that contains other components.
///
/// Children are rendered in order, vertically stacked.
#[derive(Clone)]
pub struct Container {
    children: Arc<Mutex<Vec<Arc<dyn Component>>>>,
    /// When set, only the last N children are laid out. A session with tens of
    /// thousands of transcript rows otherwise costs one full re-layout per
    /// frame — `sample` on a wedged ki showed the render thread pinned in
    /// malloc/memmove while the key reader starved on the terminal lock.
    render_tail: Option<usize>,
}

impl Container {
    /// Create a new empty container.
    pub fn new() -> Self {
        Self {
            children: Arc::new(Mutex::new(Vec::new())),
            render_tail: None,
        }
    }

    /// A container that lays out at most the last `tail` children.
    ///
    /// The transcript uses this: rendering is proportional to the visible tail
    /// rather than to the whole session, so a long run cannot starve input.
    /// Older children stay in the list (they are still navigable) — they are
    /// only skipped when drawing, behind one marker line.
    pub fn with_render_tail(tail: usize) -> Self {
        Self {
            children: Arc::new(Mutex::new(Vec::new())),
            render_tail: Some(tail.max(1)),
        }
    }

    /// Add a child component.
    pub fn add_child(&self, component: Arc<dyn Component>) {
        if let Ok(mut children) = self.children.lock() {
            children.push(component);
        }
    }

    /// Remove a child component.
    pub fn remove_child(&self, component: &Arc<dyn Component>) {
        if let Ok(mut children) = self.children.lock() {
            children.retain(|c| !Arc::ptr_eq(c, component));
        }
    }

    /// Clear all children.
    pub fn clear(&self) {
        if let Ok(mut children) = self.children.lock() {
            children.clear();
        }
    }

    /// Get the number of children.
    pub fn child_count(&self) -> usize {
        self.children.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// Get a clone of the children list.
    pub fn get_children(&self) -> Vec<Arc<dyn Component>> {
        self.children.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

impl Default for Container {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();

        if let Ok(children) = self.children.lock() {
            let skip = match self.render_tail {
                Some(tail) if children.len() > tail => children.len() - tail,
                _ => 0,
            };
            if skip > 0 {
                lines.push(format!("  … {skip} earlier entries hidden"));
            }
            for child in children.iter().skip(skip) {
                let child_lines = child.render(width);
                lines.extend(child_lines);
            }
        }

        lines
    }

    fn invalidate(&self) {
        if let Ok(children) = self.children.lock() {
            for child in children.iter() {
                child.invalidate();
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tail_tests {
    use super::*;
    use crate::text::Text;

    #[test]
    fn a_render_tail_bounds_the_laid_out_children() {
        let container = Container::with_render_tail(2);
        for index in 0..5 {
            container.add_child(Arc::new(Text::new(format!("line {index}"), 0, 0)));
        }
        let rendered = container.render(40);
        assert_eq!(rendered.len(), 3, "{rendered:?}");
        assert!(
            rendered[0].contains("3 earlier entries hidden"),
            "{rendered:?}"
        );
        assert!(rendered[1].contains("line 3"), "{rendered:?}");
        assert!(rendered[2].contains("line 4"), "{rendered:?}");
    }

    #[test]
    fn an_uncapped_container_still_renders_everything() {
        let container = Container::new();
        for index in 0..3 {
            container.add_child(Arc::new(Text::new(format!("line {index}"), 0, 0)));
        }
        assert_eq!(container.render(40).len(), 3);
    }
}
