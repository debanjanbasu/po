//! po — a native, chat-driven AI desktop harness (gpui-ce + micro-ai).
//!
//! Everything is driven from the chat: slash commands, the command palette,
//! and the UI all dispatch into the same `po-core` agent state machine.

use std::borrow::Cow;

use gpui::{
    App, Bounds, KeyBinding, QuitMode, WindowOptions, WindowBounds, actions, prelude::*, px, size,
};
use po_ui::elements::editable_text::actions::{DEFAULT_INPUT_CONTEXT, default_bindings};

use po_core::commands::CommandRegistry;
use po_ui::theme::ThemeMode;
use po_ui::font::load_fonts;
use po_ui::theme::PoTheme;
use po_ui::widgets::PoApp;

actions!(po_app, [Submit, NewSession, CommandPalette, Quit]);

fn main() {
    gpui_platform::application()
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(run);
}

/// Application bootstrap: fonts, theme, key bindings, and the main window.
fn run(cx: &mut App) {
    // Register the bundled IBM 3270 font before any window exists.
    if let Err(error) = load_fonts(cx) {
        eprintln!("po: could not register the bundled font: {error}");
    }

    // Install the noir (dark) theme as a global.
    PoTheme::install(cx, ThemeMode::Dark);

    // Text-editing key bindings for the composer, plus po's own actions.
    cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
    cx.bind_keys([
        KeyBinding::new("cmd-enter", Submit, None),
        KeyBinding::new("cmd-n", NewSession, None),
        KeyBinding::new("cmd-k", CommandPalette, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);

    let bounds = Bounds::centered(None, size(px(960.), px(680.)), cx);
    cx.open_window(
        WindowOptions::default()
            .window_bounds(WindowBounds::Windowed(bounds))
            .titlebar(gpui::TitlebarOptions::default().title(gpui::SharedString::from("po"))),
        |_window, cx| cx.new(PoApp::new),
    )
    .expect("po: failed to open the main window");
    cx.activate(true);

    // Keep the command registry warm; the shell resolves `/…` input through it.
    let _commands = CommandRegistry::new();
}

/// Handle the Submit action: send the composer text as a user message.
#[allow(dead_code)]
fn on_submit(app: &mut PoApp, cx: &mut gpui::Context<PoApp>) {
    let text = app.composer_text(cx);
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    app.push(po_core::model::ChatMessage::user(trimmed), cx);
    app.clear_composer(cx);
}

/// Silence unused-import warnings for Cow when the font path changes.
#[allow(dead_code)]
fn _font_bytes() -> Vec<Cow<'static, [u8]>> {
    vec![Cow::Borrowed(po_ui::font::IBM3270_TTF)]
}
