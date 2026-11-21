//! Toast overlay — port of `overlays/error_toast.rs` + `deep_link_toast.rs`.
//!
//! Kind → duration from `config::ui` (success 3000 / deep-link 5000 / error
//! 8000 ms), auto-dismiss via `cx.spawn` + `cx.notify`, colors from Theme.
//! Mount [`ToastHost`] as the LAST child of the root view (tree order =
//! paint order for the absolute overlay); root must be
//! `div().relative().size_full()`.

use std::time::Duration;

use gpui::{
    App, Context, Entity, FontWeight, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, prelude::*, px,
};

use crate::{
    config::ui,
    ui::theme::{Theme, shape},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Success,
    Error,
    DeepLink,
}

impl ToastKind {
    const fn duration_ms(self) -> u64 {
        match self {
            Self::Success => ui::SUCCESS_TOAST_MS,
            Self::Error => ui::ERROR_TOAST_MS,
            Self::DeepLink => ui::DEEP_LINK_TOAST_MS,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastPosition {
    Top,
    Bottom,
}

struct ToastData {
    kind: ToastKind,
    message: SharedString,
    seq: u64,
}

/// The host entity holding the current toast. Screens call
/// [`ToastHostGlobal::show`]; the root view renders the host last.
pub struct ToastHost {
    toast: Option<ToastData>,
    position: ToastPosition,
    seq: u64,
}

impl ToastHost {
    pub const fn new() -> Self {
        Self {
            toast: None,
            position: ToastPosition::Top,
            seq: 0,
        }
    }

    pub const fn position(mut self, position: ToastPosition) -> Self {
        self.position = position;
        self
    }

    pub fn show(
        &mut self,
        kind: ToastKind,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.seq += 1;
        let seq = self.seq;
        let duration = Duration::from_millis(kind.duration_ms());
        self.toast = Some(ToastData {
            kind,
            message: message.into(),
            seq,
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(duration).await;
            this.update(cx, |this, cx| {
                // Only clear if no newer toast replaced us meanwhile.
                if this.toast.as_ref().is_some_and(|t| t.seq == seq) {
                    this.toast = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

impl Render for ToastHost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::for_appearance(cx);
        let Some(toast) = &self.toast else {
            return gpui::div();
        };
        let (fill, content) = match toast.kind {
            ToastKind::Success => (theme.success.fill, theme.success.content),
            ToastKind::Error => (theme.error.fill, theme.error.content),
            ToastKind::DeepLink => (theme.primary.fill, theme.primary.content),
        };
        let anchored = match self.position {
            ToastPosition::Top => gpui::div().top_2(),
            ToastPosition::Bottom => gpui::div().bottom_2(),
        };
        anchored
            .absolute()
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .px_4()
            .child(
                gpui::div()
                    .id("toast-bubble")
                    .max_w(px(384.0))
                    .px_3()
                    .py_2()
                    .rounded(shape::RADIUS_FIELD)
                    .shadow_md()
                    .bg(fill)
                    .text_color(content)
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(toast.message.clone()),
            )
    }
}

/// Global holder so any screen can toast without prop-drilling (the port of
/// `ErrorToast`'s `error_toast()` helper + `DEEP_LINK_TOAST` signal).
pub struct ToastHostGlobal {
    pub entity: Entity<ToastHost>,
}

impl gpui::Global for ToastHostGlobal {}

/// Installs the host at boot (position: top, like the old top-strip toasts).
pub fn init_toasts(cx: &mut App) {
    let entity = cx.new(|_| ToastHost::new());
    cx.set_global(ToastHostGlobal { entity });
}

pub fn show_toast(cx: &mut App, kind: ToastKind, message: impl Into<SharedString>) {
    let entity = cx.global::<ToastHostGlobal>().entity.clone();
    entity.update(cx, |host, cx| host.show(kind, message, cx));
}

// ── Deep-link toast state (the port of overlays/deep_link_toast.rs) ─────

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Payload for the deep-link toast. Dioxus-free: both stacks read it (the
/// frozen overlay renders it; the gpui host will at the screen cutover).
#[derive(Clone)]
pub struct DeepLinkToastData {
    pub message: String,
    pub undo_input: Option<Arc<str>>,
    pub generation: u64,
}

static DEEP_LINK_TOAST: std::sync::Mutex<Option<DeepLinkToastData>> = std::sync::Mutex::new(None);
static DEEP_LINK_TOAST_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The dioxus-free clear the session phase machine calls (the old
/// `clear_deep_link_toast`).
pub fn clear_deep_link_toast() {
    let mut slot = DEEP_LINK_TOAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = None;
}

/// Snapshot the current deep-link toast, if any (the frozen overlay's read).
pub fn deep_link_toast() -> Option<DeepLinkToastData> {
    DEEP_LINK_TOAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Show a deep-link toast (success with optional undo, or info) with
/// auto-dismiss. The spawn root is the caller's: the frozen overlay passes
/// the dioxus spawn, the gpui side passes `cx.spawn`.
pub fn show_deep_link_toast_state(
    message: String,
    undo_input: Option<Arc<str>>,
    dismiss_ms: u64,
    // No `Send` bound: the wasm executors are single-threaded, and the
    // frozen overlay's dioxus spawn + the gpui `cx.spawn` both accept
    // non-`Send` futures. Native callers simply pass `Send` ones.
    spawn_root: impl Fn(std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>) + Send + 'static,
) {
    let generation = DEEP_LINK_TOAST_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    *DEEP_LINK_TOAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(DeepLinkToastData {
        message,
        undo_input,
        generation,
    });
    spawn_root(Box::pin(async move {
        crate::helpers::async_utils::sleep_ms(dismiss_ms).await;
        let should_clear = deep_link_toast()
            .as_ref()
            .is_some_and(|toast| toast.generation == generation);
        if should_clear {
            clear_deep_link_toast();
        }
    }));
}
