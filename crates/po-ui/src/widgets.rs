//! po's chat widgets: session sidebar, transcript, composer, and status bar.
//!
//! These are thin, reactive projections over `po-core` state: every widget
//! reads from entities and calls `cx.notify()` when the underlying state
//! changes, so the UI never redraws manually.

use gpui::{
    Context, Entity, Render, SharedString, Subscription, Window, div, prelude::*, px,
};
use gpui_ce_elements::editable_text::{EditableTextState, StringStorage, TextChanged, text_area};

use po_core::model::ChatMessage;

use crate::theme::PoTheme;

/// One row in the transcript.
#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    /// The message to display.
    pub message: ChatMessage,
}

/// The root view of the po window.
pub struct PoApp {
    /// Session list shown in the sidebar.
    pub sessions: Vec<String>,
    /// Transcript entries.
    pub entries: Vec<TranscriptEntry>,
    /// The composer's editable state.
    pub composer: Entity<EditableTextState>,
    /// Subscriptions kept alive for the view's lifetime.
    _subscriptions: Vec<Subscription>,
}

impl PoApp {
    /// Create the root view with an empty transcript and a fresh session.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let subscription = cx.subscribe(&composer, |_, _, _: &TextChanged, cx| cx.notify());
        Self {
            sessions: vec!["main".to_string()],
            entries: Vec::new(),
            composer,
            _subscriptions: vec![subscription],
        }
    }

    /// Append a message to the transcript.
    pub fn push(&mut self, message: ChatMessage, cx: &mut Context<Self>) {
        self.entries.push(TranscriptEntry { message });
        cx.notify();
    }

    /// The text currently in the composer.
    #[must_use]
    pub fn composer_text(&self, cx: &App) -> String {
        self.composer.read(cx).as_str().to_string()
    }

    /// Clear the composer.
    pub fn clear_composer(&mut self, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| {
            state.emplace("", cx);
        });
        cx.notify();
    }

    fn sidebar(&self, theme: &PoTheme) -> impl IntoElement {
        div()
            .id("po-sidebar")
            .w(px(220.))
            .h_full()
            .flex()
            .flex_col()
            .bg(theme.base_200)
            .border_r_1()
            .border_color(theme.base_300)
            .p_3()
            .gap_2()
            .child(
                div()
                    .text_color(theme.primary)
                    .font_family(theme.font_family.clone())
                    .text_lg()
                    .child("po"),
            )
            .children(self.sessions.iter().map(|session| {
                div()
                    .id(SharedString::from(format!("session-{session}")))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_color(theme.content)
                    .font_family(theme.font_family.clone())
                    .child(session.clone())
            }))
    }

    fn transcript(&self, theme: &PoTheme) -> impl IntoElement {
        div()
            .id("po-transcript")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .children(self.entries.iter().map(|entry| {
                let (bg, fg) = match entry.message.role {
                    po_core::model::ChatRole::User => (theme.primary, theme.primary_content),
                    po_core::model::ChatRole::Assistant => (theme.base_200, theme.content),
                    po_core::model::ChatRole::System => (theme.base_300, theme.content),
                    po_core::model::ChatRole::Tool => (theme.neutral, theme.neutral_content),
                };
                div()
                    .max_w(px(560.))
                    .when(matches!(entry.message.role, po_core::model::ChatRole::User), |d| d.self_end())
                    .when(matches!(entry.message.role, po_core::model::ChatRole::Assistant), |d| d.self_start())
                    
                    .bg(bg)
                    .text_color(fg)
                    .rounded_lg()
                    .p_3()
                    .font_family(theme.font_family.clone())
                    .child(entry.message.content.clone())
            }))
            .child(
                div()
                    .text_color(theme.base_300)
                    .font_family(theme.font_family.clone())
                    .child(if self.entries.is_empty() {
                        "po — type a message, or /help for commands."
                    } else {
                        ""
                    }),
            )
    }

    fn composer(&self, theme: &PoTheme) -> impl IntoElement {
        div()
            .id("po-composer")
            .border_t_1()
            .border_color(theme.base_300)
            .bg(theme.base_200)
            .p_3()
            .child(
                text_area("po-composer-input")
                    .state(self.composer.downgrade())
                    .placeholder("message po…  (/ for commands)")
                    .placeholder_color(theme.base_300)
                    .w_full()
                    .h(px(72.))
                    .overflow_y_scroll()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.base_300)
                    .bg(theme.base_100)
                    .text_color(theme.content)
                    .font_family(theme.font_family.clone()),
            )
    }

    fn status_bar(&self, theme: &PoTheme) -> impl IntoElement {
        div()
            .id("po-status")
            .h(px(28.))
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .bg(theme.neutral)
            .text_color(theme.neutral_content)
            .text_xs()
            .font_family(theme.font_family.clone())
            .child(format!("session: {}", self.sessions.first().map_or("—", String::as_str)))
            .child(match theme.mode {
                crate::theme::ThemeMode::Dark => "theme: dark (noir)",
                crate::theme::ThemeMode::Light => "theme: light",
            })
    }
}

impl Render for PoApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = PoTheme::current(cx).clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.base_100)
            .text_color(theme.content)
            .font_family(theme.font_family.clone())
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .min_h_0()
                    .child(self.sidebar(&theme))
                    .child(self.transcript(&theme)),
            )
            .child(self.composer(&theme))
            .child(self.status_bar(&theme))
    }
}

// Re-export for convenience.
use gpui::App;
