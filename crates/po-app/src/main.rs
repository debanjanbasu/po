//! po — a native, chat-driven AI desktop harness (gpui-ce + nano-ai).
//!
//! Everything is driven from the chat: slash commands, the command palette,
//! and the UI all dispatch into the same `po-core` agent state machine.

use gpui::{
    App, Bounds, KeyBinding, QuitMode, TitlebarOptions, WindowOptions, WindowBounds, SharedString,
    prelude::*, px, size,
};
use gpui_ce_elements::editable_text::actions::{DEFAULT_INPUT_CONTEXT, default_bindings};
use tokio::sync::mpsc;

use po_ui::font::load_fonts;
use po_ui::theme::PoTheme;
use po_ui::widgets::{PoApp, po_app_actions::{CommandPalette, NewSession, Quit, Submit}};

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
    PoTheme::install(cx, gpui_theme::ThemeMode::Dark);

    // Text-editing key bindings for the composer, plus po's own actions.
    cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
    cx.bind_keys([
        KeyBinding::new("cmd-enter", Submit, None),
        KeyBinding::new("cmd-n", NewSession, None),
        KeyBinding::new("cmd-k", CommandPalette, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);

    // Streamed model pieces flow from the async turn task into the view.
    let (pieces_tx, mut pieces_rx) = mpsc::unbounded_channel::<String>();

    let bounds = Bounds::centered(None, size(px(960.), px(680.)), cx);
    cx.open_window(
        WindowOptions::default()
            .window_bounds(WindowBounds::Windowed(bounds))
            .titlebar(TitlebarOptions {
                title: Some(SharedString::from("po")),
                ..Default::default()
            }),
        |_window, cx| {
            let provider = default_provider();
            cx.new(|cx| {
                // Drain the piece channel into the view: every delta appends
                // to the trailing assistant entry and re-renders.
                let tx = pieces_tx.clone();
                cx.spawn(async move |view, cx| {
                    // Drain provider pieces into the view.
                    while let Some(piece) = pieces_rx.recv().await {
                        let _ = view.update(cx, |this: &mut PoApp, cx| {
                            this.dispatch(po_core::agent::AgentEvent::StreamDelta { delta: piece }, cx);
                        });
                    }
                })
                .detach();

                // Promote the echo provider to the in-process engine when the
                // feature is compiled in and a model is available.
                #[cfg(all(target_os = "macos", feature = "local-engine"))]
                cx.spawn(async move |view, cx| {
                    promote_engine(po_core::config::default_model_path(), view, cx).await;
                })
                .detach();
                PoApp::with_provider(cx, tx, provider)
            })
        },
    )
    .expect("po: failed to open the main window");
    cx.activate(true);
}

/// A tiny module alias so widgets can re-use the theme's mode enum without a
/// circular dependency.
mod gpui_theme {
    pub use po_ui::theme::ThemeMode;
}

/// Select the provider at boot: the in-process nano engine when compiled in
/// and a model is present, else the deterministic echo provider.
fn default_provider() -> std::sync::Arc<dyn po_core::provider::ChatProvider> {
    std::sync::Arc::new(po_core::provider::MockEchoProvider::default())
}

/// Try to promote the provider to the in-process nano engine in the
/// background; silently keeps the echo provider when unavailable.
#[cfg(all(target_os = "macos", feature = "local-engine"))]
async fn promote_engine(
    model_path: String,
    view: gpui::WeakEntity<po_ui::widgets::PoApp>,
    cx: &mut gpui::AsyncApp,
) {
    match po_core::nanoai::LocalEngine::open(&model_path).await {
        Ok(engine) => {
            let provider = std::sync::Arc::new(po_core::provider::LocalNanoProvider {
                engine: std::sync::Arc::new(engine),
                max_new_tokens: 512,
            });
            let _ = view.update(cx, |this, cx| {
                this.provider = provider;
                cx.notify();
            });
        }
        Err(error) => {
            eprintln!("po: nano engine unavailable: {error}");
        }
    }
}
