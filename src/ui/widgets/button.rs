//! Button — gpui port of daisyUI `btn` variants (primary/ghost/danger).
use gpui::{
    App, ClickEvent, ElementId, FontWeight, IntoElement, ParentElement, RenderOnce, SharedString,
    Styled, Window, prelude::*, px,
};

use crate::ui::theme::{Theme, shape};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    /// Old `btn-primary`/`btn-secondary` (fill + readable content color).
    Primary,
    /// Old `btn-ghost` (transparent, hovers to `base_300`).
    Ghost,
    /// Destructive actions.
    Danger,
}

/// The click-handler box (shared by [`Button`] and [`Modal`]).
pub type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    variant: ButtonVariant,
    disabled: bool,
    full_width: bool,
    on_click: OnClick,
}

impl Button {
    pub fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        variant: ButtonVariant,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            variant,
            disabled: false,
            full_width: false,
            on_click: Box::new(on_click),
        }
    }

    // Builder setters: `mut self` by design (fluent chaining, like gpui's
    // own). const-able since the fields are plain bools.
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub const fn full_width(mut self, full_width: bool) -> Self {
        self.full_width = full_width;
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::for_appearance(cx);
        let (fill, content, hover) = match self.variant {
            ButtonVariant::Primary => (Some(theme.primary.fill), theme.primary.content, None),
            ButtonVariant::Ghost => (None, theme.base_content, Some(theme.base_300)),
            ButtonVariant::Danger => (Some(theme.error.fill), theme.error.content, None),
        };

        let disabled = self.disabled;
        let user_on_click = self.on_click;

        let mut btn = gpui::div()
            .id(self.id) // stateful: enables on_click + preserves the e2e id
            .flex()
            .items_center()
            .justify_center()
            .px_3()
            .h(px(32.0))
            .rounded(shape::RADIUS_FIELD)
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .when(self.full_width, gpui::Styled::w_full)
            .when_some(fill, |b, fill| b.bg(fill).text_color(content))
            .hover_style(move |s| match hover {
                Some(h) => s.bg(h),
                None => s.opacity(0.85), // fill variants dim on hover
            })
            .active_style(|s| s.opacity(0.75))
            // Disabled: guard the callback (upstream div has no disabled flag)
            // and paint the old daisyUI muted state.
            .on_click(move |ev, window, cx| {
                if !disabled {
                    user_on_click(ev, window, cx);
                }
            })
            .when(self.disabled, |b| b.opacity(0.5).cursor_default());
        if hover.is_none() {
            btn = btn.text_color(content);
        }
        btn.child(self.label)
    }
}
