//! Window chrome: the band the page keeps where the window's frame used to be.
//!
//! `platform::window` made every desktop window borderless; this draws what
//! the page has to supply now, identically on macOS, Windows and Linux — the
//! app does not adopt a platform's window furniture. That is a strip that
//! moves the window (and maximizes it on a double click, the gesture every
//! title bar carries), the caption buttons no frame draws any more, and the
//! resize edges Linux loses with its frame (Windows keeps its own frame's hit
//! testing, macOS keeps the frame it never dropped).
//!
//! Every desktop build reserves the band, which is why the shell marks itself
//! with `data-window-chrome` and `overrides.css` owns the geometry
//! (`.window-chrome`, `.window-caption-cluster`): the app's own top strip and
//! the landscape rail move below the band there, and nothing the page draws
//! may land in it.

use dioxus::prelude::*;

#[cfg(all(feature = "desktop", target_os = "macos"))]
use dioxus::desktop::LogicalPosition;
#[cfg(all(feature = "desktop", target_os = "linux"))]
use dioxus::desktop::tao::window::ResizeDirection;
#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
use dioxus::desktop::{
    DesktopContext,
    tao::event::{Event, WindowEvent},
    use_window, use_wry_event_handler,
};
#[cfg(all(feature = "desktop", target_os = "linux"))]
use dioxus::logger::tracing::warn;

/// The page's half of the borderless window.
///
/// Renders nothing where the platform has no window of its own to decorate:
/// web (the browser owns its chrome) and the phones (their activity hides the
/// system bars instead — see `native/android/MainActivity.kt`).
#[component]
pub fn WindowChrome() -> Element {
    // `platform::window::BORDERLESS` is the marker the shell stamps on the
    // root element; the same cfg here keeps the page's chrome drawn exactly
    // on the platforms whose band that marker reserves.
    #[cfg(all(
        feature = "desktop",
        any(target_os = "macos", target_os = "windows", target_os = "linux")
    ))]
    {
        let _ = crate::platform::window::BORDERLESS;
        desktop_chrome()
    }
    #[cfg(not(all(
        feature = "desktop",
        any(target_os = "macos", target_os = "windows", target_os = "linux")
    )))]
    {
        rsx! {}
    }
}

#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
fn desktop_chrome() -> Element {
    let desktop = use_window();

    // The window's state is the OS's, not ours: a snap, a keyboard maximize
    // and the caption button all change it from different places, so the icon
    // follows the window rather than the last click.
    let mut maximized = use_signal(|| desktop.window.is_maximized());
    {
        let desktop = desktop.clone();
        use_wry_event_handler(move |event, _| {
            if let Event::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } = event
            {
                maximized.set(desktop.window.is_maximized());
            }
        });
    }

    // macOS is the one desktop that has to have its window moved for it; the
    // anchor lives in context so the strip and the sheet that tracks the
    // pointer can share it without either owning the other.
    #[cfg(target_os = "macos")]
    let mut anchor = use_context_provider(|| Signal::new(None::<DragAnchor>));

    // Two closures need a handle: the strip's press and its double click. The
    // second takes the one this scope held, so only one clone is made.
    let press = desktop.clone();
    let toggle = desktop;

    rsx! {
        div {
            class: "window-chrome fixed inset-x-0 top-0 z-[1200]",
            onmousedown: move |event: MouseEvent| {
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                press.drag();
                #[cfg(target_os = "macos")]
                anchor.set(drag_anchor(&press, &event));
            },
            ondoubleclick: move |_| toggle.toggle_maximized(),
        }
        div {
            class: "window-caption-cluster fixed top-0 right-0 z-[1201] flex items-stretch",
            CaptionButton { action: CaptionAction::Minimize, maximized: false }
            CaptionButton { action: CaptionAction::Maximize, maximized: maximized() }
            CaptionButton { action: CaptionAction::Close, maximized: false }
        }
        ResizeEdges {}
        DragOverlay {}
    }
}

/// Where the pointer and the window were when a drag started: screen pixels
/// for the pointer (CSS pixels, which on macOS are the window's own points)
/// and logical pixels for the window.
#[cfg(all(feature = "desktop", target_os = "macos"))]
#[derive(Clone, Copy)]
struct DragAnchor {
    pointer: (f64, f64),
    window: (f64, f64),
}

/// The anchor a macOS drag starts from, or `None` when the window's position
/// cannot be read — a drag that cannot be anchored is not started at all
/// rather than started from a guess.
#[cfg(all(feature = "desktop", target_os = "macos"))]
fn drag_anchor(desktop: &DesktopContext, event: &MouseEvent) -> Option<DragAnchor> {
    let scale = desktop.window.scale_factor();
    let position = desktop.window.outer_position().ok()?;
    let screen = event.data().screen_coordinates();
    Some(DragAnchor {
        pointer: (screen.x, screen.y),
        window: (f64::from(position.x) / scale, f64::from(position.y) / scale),
    })
}

/// The transparent sheet a macOS drag tracks the pointer across.
///
/// It is revealed for the length of the drag, and it is what keeps the drag
/// working once the pointer moves off the strip — the window follows the
/// pointer, and the sheet covers wherever it has moved to.
#[cfg(all(feature = "desktop", target_os = "macos"))]
#[component]
fn DragOverlay() -> Element {
    let desktop = use_window();
    let mut anchor = use_context::<Signal<Option<DragAnchor>>>();

    let class = if anchor.read().is_some() {
        "fixed inset-0 z-[1202]"
    } else {
        "hidden"
    };

    rsx! {
        div {
            class,
            onpointermove: move |event| {
                let Some(start) = *anchor.peek() else {
                    return;
                };
                let screen = event.data().screen_coordinates();
                let moved = LogicalPosition::new(
                    start.window.0 + (screen.x - start.pointer.0),
                    start.window.1 + (screen.y - start.pointer.1),
                );
                desktop.window.set_outer_position(moved);
            },
            onpointerup: move |_| anchor.set(None),
            onpointercancel: move |_| anchor.set(None),
        }
    }
}

/// Nothing to track where the window manager runs the drag itself.
#[cfg(all(feature = "desktop", any(target_os = "windows", target_os = "linux")))]
#[component]
fn DragOverlay() -> Element {
    rsx! {}
}

/// What a caption button does to the window.
#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptionAction {
    Minimize,
    Maximize,
    Close,
}

#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
const CAPTION_CLASS: &str =
    "window-caption-button text-base-content/70 hover:bg-base-300 hover:text-base-content";

#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
const CLOSE_CLASS: &str =
    "window-caption-button text-base-content/70 hover:bg-error hover:text-error-content";

/// A caption button: the label it reads out, the glyph it draws, and the
/// window call it makes. The same three are drawn on every desktop, so the
/// window's controls do not move when the app crosses platforms.
#[cfg(all(
    feature = "desktop",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
))]
#[component]
fn CaptionButton(action: CaptionAction, maximized: bool) -> Element {
    let desktop = use_window();

    let (label, glyph, class) = match action {
        CaptionAction::Minimize => ("Minimize window", "icon-[heroicons--minus]", CAPTION_CLASS),
        CaptionAction::Maximize if maximized => (
            "Restore window",
            "icon-[heroicons--square-2-stack]",
            CAPTION_CLASS,
        ),
        CaptionAction::Maximize => ("Maximize window", "icon-[heroicons--stop]", CAPTION_CLASS),
        CaptionAction::Close => ("Close window", "icon-[heroicons--x-mark]", CLOSE_CLASS),
    };

    rsx! {
        button {
            class,
            r#type: "button",
            aria_label: label,
            onclick: move |_| match action {
                CaptionAction::Minimize => desktop.window.set_minimized(true),
                CaptionAction::Maximize => desktop.toggle_maximized(),
                CaptionAction::Close => desktop.close(),
            },
            span { class: glyph, aria_hidden: "true" }
        }
    }
}

/// The edges an undecorated Linux window has nothing to grab.
///
/// Windows keeps `WS_SIZEBOX` and tao hit-tests it, and macOS keeps the frame
/// it never dropped, so this is Linux's alone — including Wayland, where tao
/// leaves GTK client-side decorations on precisely so a borderless window can
/// still be moved and resized. Each edge asks the window manager for the
/// resize on the press itself: a compositor wants the press to start the
/// drag, not a later pointer event.
#[cfg(all(feature = "desktop", target_os = "linux"))]
#[component]
fn ResizeEdges() -> Element {
    rsx! {
        {resize_edge("window-resize-edge window-resize-edge--n", ResizeDirection::North)}
        {resize_edge("window-resize-edge window-resize-edge--s", ResizeDirection::South)}
        {resize_edge("window-resize-edge window-resize-edge--w", ResizeDirection::West)}
        {resize_edge("window-resize-edge window-resize-edge--e", ResizeDirection::East)}
        {resize_edge("window-resize-edge window-resize-edge--nw", ResizeDirection::NorthWest)}
        {resize_edge("window-resize-edge window-resize-edge--ne", ResizeDirection::NorthEast)}
        {resize_edge("window-resize-edge window-resize-edge--sw", ResizeDirection::SouthWest)}
        {resize_edge("window-resize-edge window-resize-edge--se", ResizeDirection::SouthEast)}
    }
}

/// Nothing to draw where the frame still hit-tests its own edges.
#[cfg(all(feature = "desktop", any(target_os = "macos", target_os = "windows")))]
#[component]
fn ResizeEdges() -> Element {
    rsx! {}
}

#[cfg(all(feature = "desktop", target_os = "linux"))]
fn resize_edge(class: &'static str, direction: ResizeDirection) -> Element {
    let desktop = use_window();

    rsx! {
        div {
            class,
            onmousedown: move |_| {
                if let Err(error) = desktop.window.drag_resize_window(direction) {
                    warn!("Resizing from the window edge was refused: {error}");
                }
            },
        }
    }
}
