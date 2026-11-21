//! Coming-soon placeholder pages (the port of `components/features/mod.rs`
//! `Placeholder`/`Ai`/`Calendar`/`Calls`) — trivial `RenderOnce` shells.

use gpui::{App, FontWeight, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window};

use crate::ui::theme::Theme;
use crate::ui::widgets::page::CenteredPage;

/// The old `Placeholder { title }` — centered title + "Coming soon...".
pub struct Placeholder {
    title: SharedString,
}

impl Placeholder {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
        }
    }
}

impl RenderOnce for Placeholder {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::for_appearance(cx);
        CenteredPage::new().contained(true).child(
            gpui::div()
                .text_center()
                .w_full()
                .child(
                    gpui::div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.base_content)
                        .child(self.title),
                )
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(theme.text_dim())
                        .child("Coming soon..."),
                ),
        )
    }
}

/// `/ai`
#[derive(IntoElement)]
pub struct Ai;

impl RenderOnce for Ai {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        Placeholder::new("AI").render(window, cx)
    }
}

/// `/calendar`
#[derive(IntoElement)]
pub struct Calendar;

impl RenderOnce for Calendar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        Placeholder::new("Calendar").render(window, cx)
    }
}

/// `/calls`
#[derive(IntoElement)]
pub struct Calls;

impl RenderOnce for Calls {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        Placeholder::new("Calls").render(window, cx)
    }
}
