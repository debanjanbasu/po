//! Modal — in-window overlay port of `components/shared/modal_dialog.rs`.
//!
//! Deliberately NOT `WindowKind::Dialog`: the web target owns one canvas and
//! popup windows are unsupported there. An absolute overlay inside the root
//! (`div().relative().size_full()`) paints above content by tree order and
//! keeps the old `<dialog open>` + esc + backdrop-click semantics; the
//! `modal=open|closed` seam marker replaces the harness's `dialog[open]`
//! selector.

use std::rc::Rc;

use gpui::{
    App, ElementId, FocusHandle, FontWeight, IntoElement, ParentElement, RenderOnce, SharedString,
    Styled, Window, hsla, prelude::*, px,
};

use crate::ui::theme::{Theme, shape};

/// The close-handler alias.
pub type OnClose = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct Modal {
    id: ElementId,
    title: SharedString,
    focus: FocusHandle,
    show_close_button: bool,
    close_disabled: bool,
    close_label: SharedString,
    panel: gpui::AnyElement,
    on_close: OnClose,
}

impl Modal {
    pub fn new(
        id: impl Into<ElementId>,
        title: impl Into<SharedString>,
        focus: FocusHandle,
        on_close: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            focus,
            show_close_button: true,
            close_disabled: false,
            close_label: "Close dialog".into(),
            panel: gpui::div().into_any_element(),
            on_close: Rc::new(on_close),
        }
    }

    pub const fn show_close_button(mut self, show: bool) -> Self {
        self.show_close_button = show;
        self
    }

    pub const fn close_disabled(mut self, disabled: bool) -> Self {
        self.close_disabled = disabled;
        self
    }

    pub fn close_label(mut self, label: impl Into<SharedString>) -> Self {
        self.close_label = label.into();
        self
    }

    pub fn panel(mut self, panel: impl IntoElement) -> Self {
        self.panel = panel.into_any_element();
        self
    }
}

impl RenderOnce for Modal {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = Theme::for_appearance(cx);
        let close_disabled = self.close_disabled;
        let on_close = self.on_close.clone();

        let esc_close = on_close.clone();
        let out_close = on_close.clone();
        let x_close = on_close.clone();

        // Overlay: absolute within the root container (root must be
        // `div().relative().size_full()`); tree order paints it above
        // content, so no z-index is needed.
        gpui::div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0., 0., 0., 0.45))
            .child(
                gpui::div()
                    .id(self.id)
                    .track_focus(&self.focus) // receive Esc while open
                    .on_key_down(move |ev, window, cx| {
                        if ev.keystroke.key == "escape" && !close_disabled {
                            esc_close(window, cx);
                        }
                    })
                    // Backdrop click = any mouse-down outside the card.
                    .on_mouse_down_out(move |_ev, window, cx| {
                        if !close_disabled {
                            out_close(window, cx);
                        }
                    })
                    .flex()
                    .flex_col()
                    .w(px(384.0))
                    .max_w_full()
                    .bg(theme.base_100)
                    .border_1()
                    .border_color(theme.input_border())
                    .rounded(shape::RADIUS_BOX)
                    .shadow_lg()
                    .p_4()
                    .child(
                        gpui::div()
                            .flex()
                            .items_start()
                            .gap_3()
                            .mb_4()
                            .child(
                                gpui::div()
                                    .flex_1()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.base_content)
                                    .child(self.title),
                            )
                            .when(self.show_close_button, |row| {
                                row.child(
                                    gpui::div()
                                        .id(SharedString::from("modal-close"))
                                        .cursor_pointer()
                                        .rounded(shape::RADIUS_FIELD)
                                        .px_2()
                                        .text_color(theme.base_content)
                                        .hover_style(|s| s.bg(theme.base_300))
                                        .on_click(move |_ev, window, cx| {
                                            if !close_disabled {
                                                x_close(window, cx);
                                            }
                                        })
                                        .child("\u{2715}"),
                                )
                            }),
                    )
                    .child(self.panel),
            )
    }
}
