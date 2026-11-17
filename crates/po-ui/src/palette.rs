//! The command palette (cmd-k).
//!
//! A keyboard-first overlay listing every slash command; picking one dispatches
//! the same [`AgentEvent`] as typing the command in the chat.

use gpui::{Context, Render, SharedString, Window, div, prelude::*, px, relative};
use po_core::commands::CommandRegistry;

use crate::theme::PoTheme;

/// A row in the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteRow {
    /// Stable command id.
    pub id: String,
    /// Slash form, e.g. `/session new`.
    pub slash: String,
    /// One-line description.
    pub description: String,
}

/// Filter the registry's commands by a case-insensitive substring query.
#[must_use]
pub fn filter(registry: &CommandRegistry, query: &str) -> Vec<PaletteRow> {
    let query = query.trim().to_ascii_lowercase();
    registry
        .commands()
        .iter()
        .filter(|command| {
            query.is_empty()
                || command.id.to_ascii_lowercase().contains(&query)
                || command.slash.to_ascii_lowercase().contains(&query)
                || command.description.to_ascii_lowercase().contains(&query)
        })
        .map(|command| PaletteRow {
            id: command.id.clone(),
            slash: command.slash.clone(),
            description: command.description.clone(),
        })
        .collect()
}

/// The palette overlay.
pub struct CommandPalette {
    /// Current filter text.
    pub query: String,
    /// Index of the highlighted row.
    pub selected: usize,
}

impl CommandPalette {
    /// A palette with no query.
    #[must_use]
    pub fn new() -> Self {
        Self {
            query: String::new(),
            selected: 0,
        }
    }

    /// Update the query and clamp the selection to the new row count.
    pub fn set_query(&mut self, query: impl Into<String>, registry: &CommandRegistry) {
        self.query = query.into();
        let count = filter(registry, &self.query).len();
        if count == 0 {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(count - 1);
        }
    }

    /// Move the highlight down (wrapping).
    pub fn next(&mut self, registry: &CommandRegistry) {
        let count = filter(registry, &self.query).len();
        if count > 0 {
            self.selected = (self.selected + 1) % count;
        }
    }

    /// Move the highlight up (wrapping).
    pub fn previous(&mut self, registry: &CommandRegistry) {
        let count = filter(registry, &self.query).len();
        if count > 0 {
            self.selected = (self.selected + count - 1) % count;
        }
    }

    /// The currently highlighted command id, if any.
    #[must_use]
    pub fn selected_id(&self, registry: &CommandRegistry) -> Option<String> {
        filter(registry, &self.query)
            .get(self.selected)
            .map(|row| row.id.clone())
    }
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = PoTheme::current(cx).clone();
        let registry = CommandRegistry::new();
        let rows = filter(&registry, &self.query);
        div()
            .id("po-palette")
            .w(relative(0.5))
            .max_w(px(520.))
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_lg()
            .bg(theme.base_200)
            .border_1()
            .border_color(theme.base_300)
            .font_family(theme.font_family.clone())
            .shadow_lg()
            .children(rows.into_iter().enumerate().map(|(index, row)| {
                let highlighted = index == self.selected;
                div()
                    .id(SharedString::from(format!("palette-row-{index}")))
                    .cursor_pointer()
                    .flex()
                    .flex_col()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .when(highlighted, |d| d.bg(theme.primary).text_color(theme.primary_content))
                    .when(!highlighted, |d| d.text_color(theme.content))
                    .child(
                        div()
                            .text_sm()
                            .child(format!("{}  ·  {}", row.slash, row.description)),
                    )
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_lists_every_command() {
        let registry = CommandRegistry::new();
        let rows = filter(&registry, "");
        assert_eq!(rows.len(), registry.commands().len());
    }

    #[test]
    fn substring_filter_matches_id_slash_and_description() {
        let registry = CommandRegistry::new();
        assert!(filter(&registry, "theme").iter().all(|r| r.id == "theme"));
        assert!(filter(&registry, "/session").iter().all(|r| r.slash.starts_with("/session")));
        assert!(filter(&registry, "fork").iter().any(|r| r.id == "session.fork"));
        assert!(filter(&registry, "no-such-thing").is_empty());
    }

    #[test]
    fn selection_clamps_and_wraps() {
        let registry = CommandRegistry::new();
        let mut palette = CommandPalette::new();
        palette.set_query("session", &registry);
        let count = filter(&registry, "session").len();
        palette.next(&registry);
        assert_eq!(palette.selected, 1 % count);
        palette.previous(&registry);
        assert_eq!(palette.selected, 0);
        palette.previous(&registry);
        assert_eq!(palette.selected, count - 1);
    }

    #[test]
    fn selected_id_resolves_the_highlighted_command() {
        let registry = CommandRegistry::new();
        let palette = CommandPalette::new();
        assert_eq!(palette.selected_id(&registry).as_deref(), Some("model"));
    }
}
