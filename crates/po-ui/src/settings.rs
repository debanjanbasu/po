//! The settings screen.
//!
//! A plain panel over [`po_core::config::Config`]: theme, font, provider
//! label, streaming budget, and the tool-approval switch. Every control
//! dispatches through the same event path as chat commands.

use gpui::{Context, Render, Window, div, prelude::*};
use po_core::config::{Config, ThemeMode};

use crate::theme::PoTheme;

/// The settings panel.
pub struct SettingsScreen {
    /// Working copy of the configuration.
    pub config: Config,
}

impl SettingsScreen {
    /// Build the panel from the current config.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Flip the theme and remember it.
    pub fn toggle_theme(&mut self) {
        self.config.theme = match self.config.theme {
            ThemeMode::Light => ThemeMode::Dark,
            ThemeMode::Dark => ThemeMode::Light,
        };
    }
}

impl Render for SettingsScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = PoTheme::current(cx).clone();
        let mode_label = match self.config.theme {
            ThemeMode::Dark => "dark (noir)",
            ThemeMode::Light => "light",
        };
        div()
            .id("po-settings")
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .bg(theme.base_100)
            .text_color(theme.content)
            .font_family(theme.font_family.clone())
            .child(
                div()
                    .text_xl()
                    .text_color(theme.primary)
                    .child("settings"),
            )
            .child(
                div()
                    .id("po-settings-theme")
                    .cursor_pointer()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(theme.base_200)
                    .border_1()
                    .border_color(theme.base_300)
                    .child(format!("theme: {mode_label} (click to toggle)"))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.toggle_theme();
                        PoTheme::install(cx, match this.config.theme {
                            po_core::config::ThemeMode::Dark => crate::theme::ThemeMode::Dark,
                            po_core::config::ThemeMode::Light => crate::theme::ThemeMode::Light,
                        });
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(theme.base_200)
                    .border_1()
                    .border_color(theme.base_300)
                    .child(format!("provider: {}", self.config.provider.label())),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(theme.base_200)
                    .border_1()
                    .border_color(theme.base_300)
                    .child(format!("font: {}", self.config.font_family)),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(theme.base_200)
                    .border_1()
                    .border_color(theme.base_300)
                    .child(format!(
                        "approve tools: {}",
                        if self.config.approve_tools { "on" } else { "off" }
                    )),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(theme.base_200)
                    .border_1()
                    .border_color(theme.base_300)
                    .child(format!("max new tokens: {}", self.config.max_new_tokens)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_toggle_flips_both_ways() {
        let mut screen = SettingsScreen::new(Config::default());
        assert_eq!(screen.config.theme, ThemeMode::Dark);
        screen.toggle_theme();
        assert_eq!(screen.config.theme, ThemeMode::Light);
        screen.toggle_theme();
        assert_eq!(screen.config.theme, ThemeMode::Dark);
    }
}
