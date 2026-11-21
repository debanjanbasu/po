//! `CenteredPage` + `SectionHeader` — port of `components/shared/shared_widgets.rs`.
//!
//! `contained: false` (default): full-screen shell, scrolls when content
//! exceeds the viewport (old `h-dvh overflow-y-auto grid place-items-center`).
//! `contained: true`: fills the available layout height so flex pages can
//! own their scroll regions (old `h-full min-h-0 grid place-items-center`).

use gpui::{
    AnyElement, App, FontWeight, IntoElement, ParentElement, Pixels, RenderOnce, SharedString,
    Styled, Window, prelude::*, px,
};

use crate::ui::theme::Theme;

#[derive(IntoElement)]
pub struct CenteredPage {
    /// Inner content width (old `width` class; `max-w-sm` = 384).
    width: Pixels,
    contained: bool,
    children: Vec<AnyElement>,
}

impl CenteredPage {
    pub const fn new() -> Self {
        Self {
            width: px(384.0),
            contained: false,
            children: Vec::new(),
        }
    }

    pub const fn width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    pub const fn contained(mut self, contained: bool) -> Self {
        self.contained = contained;
        self
    }

    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }

    pub fn children(mut self, children: impl IntoIterator<Item = impl IntoElement>) -> Self {
        self.children
            .extend(children.into_iter().map(IntoElement::into_any_element));
        self
    }
}

impl Default for CenteredPage {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderOnce for CenteredPage {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mut shell = gpui::div()
            .id("centered-page")
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .w_full()
            .h_full()
            .px_4();
        if self.contained {
            // Contained: hand overflow control to the flex parent.
            shell = shell.py_6();
        } else {
            // Full-screen: this shell scrolls. Safe-area padding comes from
            // platform insets at the root (CSS `env()` no longer applies).
            shell = shell.overflow_y_scroll();
        }
        shell.child(
            gpui::div()
                .w_full()
                .max_w(self.width)
                .when(self.contained, gpui::Styled::h_full)
                .flex()
                .flex_col()
                .children(self.children),
        )
    }
}

/// Section title + optional subtitle (settings screens).
#[derive(IntoElement)]
pub struct SectionHeader {
    title: SharedString,
    subtitle: Option<SharedString>,
}

impl SectionHeader {
    pub fn new(title: impl Into<SharedString>) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
        }
    }

    pub fn subtitle(mut self, subtitle: impl Into<SharedString>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }
}

impl RenderOnce for SectionHeader {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::for_appearance(cx);
        gpui::div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                gpui::div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.base_content)
                    .child(self.title),
            )
            .children(
                self.subtitle
                    .map(|s| gpui::div().text_sm().text_color(theme.text_dim()).child(s)),
            )
    }
}
