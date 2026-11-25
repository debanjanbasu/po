//! Shared QR acquisition controls (camera scan + image upload).

use std::rc::Rc;

use dioxus::prelude::*;

use crate::{
    helpers::spawn, platform::qr_scanner, session::epoch::SessionGuard,
    storage::background_errors::report_nonfatal,
};

use super::QrScannerOverlay;

/// Renders join-compatible buttons for camera scan and image upload,
/// and manages a fullscreen scanner overlay for camera-based QR capture.
#[component]
pub fn QrAcquireButtons(
    on_result: EventHandler<String>,
    on_error: Option<EventHandler<String>>,
    #[props(default)] disabled: bool,
    #[props(default = "Scan QR code")] scan_aria_label: &'static str,
    #[props(default = "Upload QR image")] upload_aria_label: &'static str,
) -> Element {
    const TRIGGER_CLASS: &str = "btn btn-sm btn-secondary btn-square join-item";

    let mut show_scanner = use_signal(|| false);
    let mut scan_btn: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    let mut scan_guard = use_signal(SessionGuard::capture);
    let mut uploading = use_signal(|| false);
    let mut upload_error = use_signal(|| None::<String>);
    let can_scan = use_resource(qr_scanner::can_scan_async);

    // File upload via <input type="file"> works on web and desktop webviews but
    // not on mobile (iOS/Android) where the Dioxus file bridge cannot deliver bytes.
    let show_upload = cfg!(target_arch = "wasm32")
        || cfg!(target_os = "macos")
        || cfg!(target_os = "linux")
        || cfg!(target_os = "windows");
    let spinner_class = if uploading() {
        "loading loading-spinner loading-xs"
    } else {
        "icon-[line-md--image-twotone]"
    };

    rsx! {
        if can_scan().unwrap_or(false) {
            button {
                r#type: "button",
                disabled: disabled || uploading(),
                class: TRIGGER_CLASS,
                "aria-label": scan_aria_label,
                onmounted: move |ev| scan_btn.set(Some(ev.data())),
                onclick: move |_| {
                    if disabled || *uploading.peek() || *show_scanner.peek() {
                        return;
                    }
                    upload_error.set(None);
                    scan_guard.set(SessionGuard::capture());
                    show_scanner.set(true);
                },
                span {
                    class: "icon-[heroicons--qr-code]",
                    "aria-hidden": "true",
                }
            }
        }

        if show_upload {
            label {
                class: "btn btn-sm btn-secondary btn-square join-item focus-within:outline focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-secondary",
                "aria-busy": if uploading() { "true" } else { "false" },
                input {
                    r#type: "file",
                    aria_label: upload_aria_label,
                    accept: "image/png,image/jpeg,image/webp",
                    class: "sr-only",
                    disabled: disabled || uploading(),
                    onchange: move |e: Event<FormData>| {
                        if disabled || *uploading.peek() {
                            return;
                        }
                        let Some(first) = e.files().into_iter().next() else {
                            return;
                        };
                        uploading.set(true);
                        upload_error.set(None);
                        let guard = SessionGuard::capture();
                        spawn(async move {
                            let result = match first.read_bytes().await {
                                Ok(bytes) => qr_scanner::decode_image_bytes_async(bytes)
                                    .await
                                    .ok_or("No readable QR code found in the uploaded image"),
                                Err(error) => {
                                    guard.commit(|| {
                                        report_nonfatal(
                                            "components.qr_acquire",
                                            "Failed to read uploaded image",
                                            &error,
                                        );
                                    });
                                    Err("Failed to read the uploaded image. Please try again.")
                                }
                            };
                            guard.commit(|| {
                                uploading.set(false);
                                match result {
                                    Ok(content) => on_result.call(content),
                                    Err(message) if let Some(handler) = on_error => {
                                        handler.call(message.to_string());
                                    }
                                    Err(message) => {
                                        upload_error.set(Some(message.to_string()));
                                    }
                                }
                            });
                        });
                    },
                }
                span {
                    class: spinner_class,
                    "aria-hidden": "true",
                }
                if uploading() {
                    span { class: "sr-only", role: "status", "Reading QR image..." }
                }
            }
        }

        if let Some(error) = upload_error() {
            span { class: "text-error text-xs self-center ml-2", role: "alert", "{error}" }
        }

        if show_scanner() {
            QrScannerOverlay {
                on_result: move |result: Option<String>| {
                    let guard = *scan_guard.peek();
                    show_scanner.set(false);
                    if let Some(btn) = scan_btn.read().clone() {
                        spawn(async move {
                            let _ = btn.set_focus(true).await;
                        });
                    }
                    if let Some(content) = result {
                        guard.commit(|| on_result.call(content));
                    }
                }
            }
        }
    }
}
