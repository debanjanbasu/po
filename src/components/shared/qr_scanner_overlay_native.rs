//! Native half of the QR scanner overlay.
//!
//! `qr_scanner_overlay` keeps the component and its wasm (getUserMedia)
//! branch; this file holds the native render branch only. On native targets
//! the actual scanning is delegated to the platform FFI scanner
//! (`platform::qr_scanner`), so the overlay here is the always-on-the-screen
//! half: the fullscreen spinner, its "Camera scanner active…" line, and the
//! Cancel control (click, Escape, Tab-trap).

#[cfg(not(target_arch = "wasm32"))]
use std::rc::Rc;

#[cfg(not(target_arch = "wasm32"))]
use dioxus::prelude::*;

#[cfg(not(target_arch = "wasm32"))]
use crate::components::shared::{Spinner, SpinnerSize};

/// The native scanner overlay: a blocking full-screen dialog while the
/// platform FFI scan runs. Focus starts on the Cancel button and Tab is
/// trapped on it, because there is nothing else to reach.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn render_native_overlay(
    mut cancel_btn: Signal<Option<Rc<MountedData>>>,
    cancel_click: impl FnMut(MouseEvent) + Copy + 'static,
    cancel_key: impl FnMut(KeyboardEvent) + Copy + 'static,
) -> Element {
    rsx! {
        div {
            class: "fixed inset-0 z-[1200] bg-black/80 grid place-items-center",
            role: "dialog",
            "aria-modal": "true",
            onkeydown: cancel_key,
            tabindex: "0",
            autofocus: true,
            div { class: "flex flex-col items-center gap-4",
                Spinner { size: SpinnerSize::Lg, class: "text-white" }
                p { class: "text-white", "Camera scanner active…" }
                button {
                    class: "btn",
                    onmounted: move |ev| cancel_btn.set(Some(ev.data())),
                    onclick: cancel_click,
                    "Cancel"
                }
            }
        }
    }
}
