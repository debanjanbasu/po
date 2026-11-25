//! Auto-dismiss toast for deep-link actions: success with optional undo, or
//! an informational notice.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use dioxus::prelude::*;

use crate::{
    chat::session_flow::undo_add_contact,
    components::overlays::error_toast::{ToastKind, ToastShell},
    config::ui::DEEP_LINK_TOAST_MS as AUTO_DISMISS_MS,
    helpers::{async_utils::sleep_ms, spawn},
    nonfatal,
};

/// Payload for the deep-link toast.
#[derive(Clone)]
pub struct DeepLinkToastData {
    pub message: String,
    pub undo_input: Option<Arc<str>>,
    pub(super) kind: ToastKind,
    generation: u64,
}

pub static DEEP_LINK_TOAST: GlobalSignal<Option<DeepLinkToastData>> = Signal::global(|| None);
static DEEP_LINK_TOAST_GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn clear_deep_link_toast() {
    *DEEP_LINK_TOAST.write() = None;
}

/// Show a deep-link success toast with auto-dismiss and optional undo.
pub fn show_deep_link_toast(message: String, undo_input: Option<Arc<str>>) {
    show_toast(message, undo_input, ToastKind::Success);
}

/// Show an informational deep-link notice (no undo action).
pub fn show_deep_link_notice(message: String) {
    show_toast(message, None, ToastKind::Info);
}

fn show_toast(message: String, undo_input: Option<Arc<str>>, kind: ToastKind) {
    let generation = DEEP_LINK_TOAST_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    *DEEP_LINK_TOAST.write() = Some(DeepLinkToastData {
        message,
        undo_input,
        kind,
        generation,
    });
    spawn(async move {
        sleep_ms(AUTO_DISMISS_MS).await;
        let should_clear = DEEP_LINK_TOAST
            .read()
            .as_ref()
            .is_some_and(|toast| toast.generation == generation);
        if should_clear {
            clear_deep_link_toast();
        }
    });
}

/// Renders the deep-link toast with optional undo button.
#[component]
pub fn DeepLinkToast() -> Element {
    let data = DEEP_LINK_TOAST.read();
    let Some(toast) = data.as_ref() else {
        return rsx! {};
    };
    let msg = toast.message.clone();
    let undo_input = toast.undo_input.clone();
    let kind = toast.kind;

    rsx! {
        ToastShell { kind,
            span { "{msg}" }
            div { class: "flex gap-1",
                if let Some(input) = undo_input {
                    button {
                        class: "btn btn-sm btn-ghost",
                        "aria-label": "Undo adding contact",
                        onclick: move |_| {
                            let input = input.clone();
                            clear_deep_link_toast();
                            spawn(async move {
                                nonfatal!(
                                    "deep_link_toast.undo",
                                    "Failed to undo contact add",
                                    undo_add_contact(&input).await
                                );
                            });
                        },
                        "Undo"
                    }
                }
                button {
                    class: "btn btn-sm btn-ghost btn-circle",
                    "aria-label": "Dismiss",
                    onclick: move |_| {
                        clear_deep_link_toast();
                    },
                    span {
                        class: "icon-[line-md--close-square-twotone]",
                        "aria-hidden": "true",
                    }
                }
            }
        }
    }
}
