//! po's chat widgets: session sidebar, transcript, composer, and status bar.
//!
//! These are thin, reactive projections over `po-core` state: every widget
//! reads from entities and calls `cx.notify()` when the underlying state
//! changes, so the UI never redraws manually.

use gpui::{
    App, Context, Entity, Render, SharedString, Subscription, Window, div, prelude::*, px,
};
use gpui_ce_elements::editable_text::{
    EditableTextState, StringStorage, TextChanged, text_area,
};

use po_core::agent::{AgentEvent, AgentState, transition};
use po_core::app_mode::{AppEvent, AppMode};
use po_core::commands::CommandRegistry;
use po_core::model::ChatMessage;
use tokio::sync::mpsc::UnboundedSender;

use crate::theme::{PoTheme, ThemeMode};

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
    /// Which screen is active.
    pub mode: AppMode,
    /// The agent lifecycle state, projected into the UI.
    pub agent: AgentState,
    /// Login/signup form fields (only meaningful on auth screens).
    pub username: Entity<EditableTextState>,
    pub password: Entity<EditableTextState>,
    /// Transient notices shown above the composer.
    pub notice: Option<String>,
    /// Channel the async turn task streams pieces into.
    pub pieces: UnboundedSender<String>,
    /// Slash-command registry shared by chat and palette.
    pub commands: CommandRegistry,
    /// Subscriptions kept alive for the view's lifetime.
    _subscriptions: Vec<Subscription>,
}

impl PoApp {
    /// Create the root view with an empty transcript and a fresh session.
    pub fn new(cx: &mut Context<Self>, pieces: UnboundedSender<String>) -> Self {
        let composer = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let username = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let password = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let subscriptions = vec![
            cx.subscribe(&composer, |_, _, _: &TextChanged, cx| cx.notify()),
            cx.subscribe(&username, |_, _, _: &TextChanged, cx| cx.notify()),
            cx.subscribe(&password, |_, _, _: &TextChanged, cx| cx.notify()),
        ];
        Self {
            sessions: vec!["main".to_string()],
            entries: Vec::new(),
            composer,
            mode: AppMode::Login,
            agent: AgentState::Idle,
            username,
            password,
            notice: None,
            pieces,
            commands: CommandRegistry::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Append a message to the transcript.
    pub fn push(&mut self, message: ChatMessage, cx: &mut Context<Self>) {
        self.entries.push(TranscriptEntry { message });
        cx.notify();
    }

    /// Push a system notice line.
    pub fn notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.push(ChatMessage::system(text), cx);
    }

    /// The text currently in the composer.
    #[must_use]
    pub fn composer_text(&self, cx: &App) -> String {
        self.composer.read(cx).as_str().to_string()
    }

    /// Read a field's text and clear it.
    fn take_field(&self, field: &Entity<EditableTextState>, cx: &mut Context<Self>) -> String {
        let text = field.read(cx).as_str().trim().to_string();
        field.update(cx, |state, cx| state.emplace("", cx));
        text
    }

    /// Clear the composer.
    pub fn clear_composer(&mut self, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| {
            state.emplace("", cx);
        });
        cx.notify();
    }

    /// Handle the Submit action: dispatch slash commands or start a turn.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.composer_text(cx);
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }

        if trimmed.starts_with('/') {
            let Some(invocation) = self.commands.parse(&trimmed) else {
                self.notice(format!("unknown command: {trimmed}"), cx);
                self.clear_composer(cx);
                return;
            };
            let Ok(event) = self.commands.event(&invocation.id, &invocation.args) else {
                self.notice(format!("unknown command: {trimmed}"), cx);
                self.clear_composer(cx);
                return;
            };
            self.clear_composer(cx);
            self.dispatch(event, cx);
            return;
        }

        self.dispatch(AgentEvent::UserMessage { content: trimmed.clone() }, cx);
        self.push(ChatMessage::user(trimmed), cx);
    }

    /// Apply an agent event through the state machine.
    pub fn dispatch(&mut self, event: AgentEvent, cx: &mut Context<Self>) {
        let next = transition(&self.agent.clone(), &event);
        self.agent = next.state;
        for effect in next.effects {
            self.apply(event_label(&event), effect, cx);
        }
        cx.notify();
    }

    fn apply(&mut self, source: &str, effect: po_core::agent::Effect, cx: &mut Context<Self>) {
        use po_core::agent::Effect;
        match effect {
            Effect::SetTheme { mode } => {
                if let Some(next) = ThemeMode::parse(&mode, PoTheme::current(cx).mode) {
                    PoTheme::install(cx, next);
                }
                self.notice(format!("theme: {mode}"), cx);
            }
            Effect::SetFont { family } => {
                self.notice(format!("font: {family}"), cx);
            }
            Effect::ClearView => self.entries.clear(),
            Effect::Quit => cx.quit(),
            Effect::AppendDelta { delta } => {
                self.append_delta(&delta, cx);
            }
            Effect::RecordMessage { message } => self.push(message, cx),
            Effect::RecordToolResult { tool, result, ok } => {
                self.notice(format!("{tool}: {result} ({})", if ok { "ok" } else { "failed" }), cx);
            }
            Effect::NewSession => {
                let name = format!("session-{}", self.sessions.len() + 1);
                self.sessions.push(name);
                self.entries.clear();
                self.notice("new session started", cx);
            }
            Effect::ListSessions => {
                let list = self.sessions.join(", ");
                self.notice(format!("sessions: {list}"), cx);
            }
            Effect::SwitchSession { id } => {
                if self.sessions.contains(&id) {
                    self.entries.clear();
                    self.notice(format!("switched to {id}"), cx);
                } else {
                    self.notice(format!("no such session: {id}"), cx);
                }
            }
            Effect::ForkSession { id } => {
                let name = if id.is_empty() {
                    format!("{}-fork", self.sessions.last().map_or("main", String::as_str))
                } else {
                    id
                };
                self.sessions.push(name.clone());
                self.notice(format!("forked into {name}"), cx);
            }
            Effect::ListTools => {
                let registry = po_core::tools::ToolRegistry::with_defaults(Vec::new());
                self.notice(format!("tools: {}", registry.names().join(", ")), cx);
            }
            Effect::SetModel { model } => {
                if model.is_empty() {
                    self.notice("model: (current provider)", cx);
                } else {
                    self.notice(format!("model switched: {model}"), cx);
                }
            }
            Effect::StartTurn { messages } => {
                self.begin_stream(source, &messages, cx);
            }
            Effect::AbortProvider => {
                self.notice("aborted", cx);
            }
            Effect::CompactContext => self.notice("compacting…", cx),
            Effect::Stopped => self.notice("po stopped", cx),
            Effect::RunTool { call } => {
                self.notice(
                    format!("tool approval: {} (run {})", call.name, call.id),
                    cx,
                );
            }
            Effect::UnknownCommand { input } => {
                self.notice(format!("unknown command: {input}"), cx);
            }
        }
        cx.notify();
    }

    /// Stream a model turn: nano-ai when available, an echo otherwise.
    fn begin_stream(&mut self, _source: &str, messages: &[ChatMessage], cx: &mut Context<Self>) {
        let prompt = po_core::nanoai_prompt(messages);
        let tx = self.pieces.clone();
        self.entries.push(TranscriptEntry {
            message: ChatMessage::assistant(String::new()),
        });
        cx.notify();
        cx.spawn(async move |view, cx| {
            // Local inference requires the opt-in `local-engine` feature and a
            // Rust >= 1.101 toolchain (see README); until then po streams an
            // echo reply so the chat flow is demonstrable everywhere.
            let reply = format!("echo: {prompt}");
            for word in reply.split_inclusive(char::is_whitespace) {
                let _ = tx.send(word.to_string());
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
            }
            let _ = tx.send(String::new());
            let _ = view.update(cx, |this, cx| {
                this.agent = po_core::agent::AgentState::Idle;
                cx.notify();
            });
        })
        .detach();
    }

    /// Append a streamed piece to the trailing assistant entry.
    fn append_delta(&mut self, delta: &str, cx: &mut Context<Self>) {
        if delta.is_empty() {
            return;
        }
        match self.entries.last_mut() {
            Some(entry) if matches!(entry.message.role, po_core::model::ChatRole::Assistant) => {
                entry.message.content.push_str(delta);
            }
            _ => self.entries.push(TranscriptEntry {
                message: ChatMessage::assistant(delta),
            }),
        }
        cx.notify();
    }

    /// Attempt to navigate via the app-mode FSM (login/signup/logout).
    pub fn handle_auth(&mut self, event: AppEvent, cx: &mut Context<Self>) {
        let (mode, effects) = po_core::app_mode::transition(&self.mode, &event);
        self.mode = mode;
        for effect in effects {
            match effect {
                po_core::app_mode::AppEffect::SetMode { mode } => {
                    self.mode = mode;
                    if mode == AppMode::Chat {
                        self.notice("welcome to po", cx);
                    }
                }
                po_core::app_mode::AppEffect::ShowMessage { text } => self.notice(text, cx),
                _ => {}
            }
        }
        cx.notify();
    }

    // ---- rendering helpers -------------------------------------------

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
            .children(self.entries.iter().filter(|entry| !entry.message.content.is_empty()).map(|entry| {
                let (bg, fg) = match entry.message.role {
                    po_core::model::ChatRole::User => (theme.primary, theme.primary_content),
                    po_core::model::ChatRole::Assistant => (theme.base_200, theme.content),
                    po_core::model::ChatRole::System => (theme.base_300, theme.content),
                    po_core::model::ChatRole::Tool => (theme.neutral, theme.neutral_content),
                };
                let align = if matches!(entry.message.role, po_core::model::ChatRole::User) {
                    "end"
                } else {
                    "start"
                };
                div()
                    .max_w(px(560.))
                    .flex()
                    .when(align == "end", |d| d.flex_row().justify_end())
                    .child(
                        div()
                            .bg(bg)
                            .text_color(fg)
                            .rounded_lg()
                            .p_3()
                            .font_family(theme.font_family.clone())
                            .child(entry.message.content.clone()),
                    )
            }))
            .when(self.entries.is_empty(), |d| {
                d.child(
                    div()
                        .text_color(theme.base_300)
                        .font_family(theme.font_family.clone())
                        .child("po — type a message, or / for commands."),
                )
            })
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

    fn auth_screen(&self, theme: &PoTheme, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("po-auth")
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(theme.base_100)
            .text_color(theme.content)
            .font_family(theme.font_family.clone())
            .child(
                div()
                    .text_color(theme.primary)
                    .text_3xl()
                    .font_family(theme.font_family.clone())
                    .child("po"),
            )
            .child(
                text_area("po-auth-username")
                    .state(self.username.downgrade())
                    .placeholder("username")
                    .placeholder_color(theme.base_300)
                    .w(px(280.))
                    .min_h_auto()
                    .max_h_auto()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.base_300)
                    .bg(theme.base_200)
                    .text_color(theme.content)
                    .font_family(theme.font_family.clone()),
            )
            .child(
                text_area("po-auth-password")
                    .state(self.password.downgrade())
                    .placeholder("password")
                    .placeholder_color(theme.base_300)
                    .w(px(280.))
                    .min_h_auto()
                    .max_h_auto()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.base_300)
                    .bg(theme.base_200)
                    .text_color(theme.content)
                    .font_family(theme.font_family.clone()),
            )
            .child(
                div()
                    .id("po-auth-submit")
                    .cursor_pointer()
                    .px_4()
                    .py_2()
                    .rounded_lg()
                    .bg(theme.primary)
                    .text_color(theme.primary_content)
                    .child(match self.mode {
                        AppMode::Signup => "create account",
                        _ => "sign in",
                    })
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        let username = this.take_field(&this.username, cx);
                        let password = this.take_field(&this.password, cx);
                        let event = if this.mode == AppMode::Signup {
                            AppEvent::SignupAttempt { username, password }
                        } else {
                            AppEvent::LoginAttempt { username, password }
                        };
                        this.handle_auth(event, cx);
                    })),
            )
            .child(
                div()
                    .id("po-auth-toggle")
                    .cursor_pointer()
                    .text_color(theme.base_300)
                    .child(if self.mode == AppMode::Signup {
                        "have an account? sign in"
                    } else {
                        "new here? create an account"
                    })
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.mode = if this.mode == AppMode::Signup {
                            AppMode::Login
                        } else {
                            AppMode::Signup
                        };
                        cx.notify();
                    })),
            )
            .when(self.mode == AppMode::Signup, |d| d)
    }

    fn status_bar(&self, theme: &PoTheme) -> impl IntoElement {
        let agent_label = match &self.agent {
            po_core::agent::AgentState::Idle => "idle".to_string(),
            po_core::agent::AgentState::Streaming { .. } => "streaming…".to_string(),
            po_core::agent::AgentState::AwaitingToolApproval { .. } => "awaiting approval".to_string(),
            po_core::agent::AgentState::RunningTool { tool, .. } => format!("running {tool}"),
            po_core::agent::AgentState::Compacting { .. } => "compacting…".to_string(),
            po_core::agent::AgentState::Stopped => "stopped".to_string(),
            po_core::agent::AgentState::Failed { .. } => "failed".to_string(),
        };
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
            .child(format!(
                "session: {} · agent: {agent_label}",
                self.sessions.first().map_or("—", String::as_str)
            ))
            .child(match theme.mode {
                ThemeMode::Dark => "theme: dark (noir)",
                ThemeMode::Light => "theme: light",
            })
    }
}

fn event_label(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::UserMessage { .. } => "user",
        _ => "command",
    }
}

impl Render for PoApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = PoTheme::current(cx).clone();
        if matches!(self.mode, AppMode::Login | AppMode::Signup) {
            return self.auth_screen(&theme, cx).into_any_element();
        }
        div()
            .id("po-root")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.base_100)
            .text_color(theme.content)
            .font_family(theme.font_family.clone())
            .on_action::<po_app_actions::Submit>(cx.listener(|this, _: &po_app_actions::Submit, _window, cx| {
                this.submit(cx);
            }))
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
            .into_any_element()
    }
}

/// Actions declared by the binary crate, re-declared here as a module alias.
pub mod po_app_actions {
    gpui::actions!(po_app, [Submit, NewSession, CommandPalette, Quit]);
}
