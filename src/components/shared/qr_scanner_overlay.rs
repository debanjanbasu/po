// NOTE: This file is intentionally kept as a single module (~313 LOC) because
// it is a single component (QrScannerOverlay) with platform-conditional logic
// (#[cfg] blocks for wasm32 vs native). Splitting by platform would duplicate
// the component signature and complicate the cfg gating.

//! Fullscreen camera QR scanner overlay.
//!
//! On WASM, uses `getUserMedia` for live camera feed with `rqrr` decode.
//! On native platforms, delegates to the existing platform FFI scanner.

use std::rc::Rc;

use dioxus::prelude::*;

use crate::{helpers::spawn, platform::qr_scanner};

#[cfg(not(target_arch = "wasm32"))]
use crate::components::shared::{Spinner, SpinnerSize};

#[cfg(target_arch = "wasm32")]
use crate::helpers::async_utils::sleep_ms;
#[cfg(target_arch = "wasm32")]
use js_sys::{Object, Reflect};
#[cfg(target_arch = "wasm32")]
use tokio::sync::Notify;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{JsCast, JsValue};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::{JsFuture, spawn_local};
#[cfg(target_arch = "wasm32")]
use web_sys::{
    CanvasRenderingContext2d, Document, HtmlCanvasElement, HtmlElement, HtmlVideoElement,
    MediaStream, MediaStreamConstraints, MediaStreamTrack, window,
};

#[cfg(target_arch = "wasm32")]
const DECODE_INTERVAL_MS: u64 = 250;
#[cfg(target_arch = "wasm32")]
const VIDEO_ELEMENT_ID: &str = "po-qr-video";
#[cfg(target_arch = "wasm32")]
const CANVAS_ELEMENT_ID: &str = "po-qr-canvas";
#[cfg(not(target_arch = "wasm32"))]
fn render_native_overlay(
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

#[cfg(target_arch = "wasm32")]
fn render_wasm_error_overlay(
    err: &str,
    mut cancel_btn: Signal<Option<Rc<MountedData>>>,
    cancel_click: impl FnMut(MouseEvent) + Copy + 'static,
    cancel_key: impl FnMut(KeyboardEvent) + Copy + 'static,
) -> Element {
    rsx! {
        div { class: "fixed inset-0 z-[1200] bg-black/85 grid place-items-center p-4", role: "dialog", "aria-modal": "true", onkeydown: cancel_key, tabindex: "0", autofocus: true,
            div { class: "w-full max-w-sm rounded-box border border-base-content/20 bg-base-100/10 backdrop-blur-sm p-4 space-y-3",
                p { class: "text-white", "Camera unavailable" }
                p { class: "text-white/80", "{err}" }
                button {
                    class: "btn btn-sm w-full",
                    onmounted: move |ev| cancel_btn.set(Some(ev.data())),
                    onclick: cancel_click,
                    "Close"
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn render_wasm_scanner_overlay(
    mut cancel_btn: Signal<Option<Rc<MountedData>>>,
    cancel_click: impl FnMut(MouseEvent) + Copy + 'static,
    cancel_key: impl FnMut(KeyboardEvent) + Copy + 'static,
) -> Element {
    rsx! {
        div {
            class: "fixed inset-0 z-[1200] bg-black overflow-hidden",
            role: "dialog",
            "aria-modal": "true",
            onkeydown: cancel_key,
            tabindex: "0",
            autofocus: true,
            video {
                id: VIDEO_ELEMENT_ID,
                class: "pointer-events-none absolute inset-0 h-full w-full object-cover",
                autoplay: true,
                playsinline: true,
                muted: true,
            }

            canvas { id: CANVAS_ELEMENT_ID, class: "hidden" }

            div { class: "pointer-events-none absolute inset-0 bg-black/25" }

            div { class: "qr-scanner-bar absolute inset-x-0 bottom-0 z-10 flex items-center justify-between px-4 pt-4 bg-linear-to-t from-black/95 via-black/75 to-transparent",
                p { class: "text-white/80", "Point camera at a QR code" }
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

#[cfg(target_arch = "wasm32")]
fn get_document() -> Option<Document> {
    window()?.document()
}

#[cfg(target_arch = "wasm32")]
fn attach_stream_to_video(stream: &MediaStream) {
    let Some(el) = get_document().and_then(|doc| doc.get_element_by_id(VIDEO_ELEMENT_ID)) else {
        return;
    };
    let Ok(video) = el.dyn_into::<HtmlVideoElement>() else {
        return;
    };
    video.set_src_object(Some(stream));
}

#[cfg(target_arch = "wasm32")]
async fn request_camera_stream() -> Option<MediaStream> {
    let window = window()?;
    let media_devices = window.navigator().media_devices().ok()?;

    let constraints = MediaStreamConstraints::new();
    let video_constraint = Object::new();
    Reflect::set(
        &video_constraint,
        &JsValue::from_str("facingMode"),
        &JsValue::from_str("environment"),
    )
    .ok()?;
    constraints.set_video(&video_constraint);
    let promise = media_devices
        .get_user_media_with_constraints(&constraints)
        .ok()?;
    JsFuture::from(promise).await.ok()?.dyn_into().ok()
}

#[cfg(target_arch = "wasm32")]
fn canvas_context() -> Option<(HtmlCanvasElement, CanvasRenderingContext2d)> {
    let el = get_document()?.get_element_by_id(CANVAS_ELEMENT_ID)?;
    let canvas: HtmlCanvasElement = el.dyn_into().ok()?;
    // The decode loop reads pixels back every frame via `get_image_data`, so hint
    // the browser to keep this canvas CPU-backed (avoids the Canvas2D
    // "willReadFrequently" readback-performance warning).
    let opts = Object::new();
    Reflect::set(
        &opts,
        &JsValue::from_str("willReadFrequently"),
        &JsValue::from_bool(true),
    )
    .ok()?;
    let ctx: CanvasRenderingContext2d = canvas
        .get_context_with_context_options("2d", &opts)
        .ok()??
        .dyn_into()
        .ok()?;
    Some((canvas, ctx))
}

#[cfg(target_arch = "wasm32")]
fn video_element() -> Option<HtmlVideoElement> {
    get_document()?
        .get_element_by_id(VIDEO_ELEMENT_ID)?
        .dyn_into()
        .ok()
}

/// True while the page is backgrounded. Decoding then is wasted work: the
/// camera keeps streaming, but no frame can be shown to the user anyway.
#[cfg(target_arch = "wasm32")]
fn page_hidden() -> bool {
    get_document().is_some_and(|doc| doc.hidden())
}

#[cfg(target_arch = "wasm32")]
fn decode_from_video_frame(
    video: &HtmlVideoElement,
    canvas: &HtmlCanvasElement,
    ctx: &CanvasRenderingContext2d,
) -> Option<String> {
    const MAX_DECODE_DIMENSION: u32 = 640;

    let vw = video.video_width();
    let vh = video.video_height();
    if vw == 0 || vh == 0 {
        return None;
    }

    let source_edge = u64::from(vw.max(vh));
    let target_edge = u64::from(vw.max(vh).min(MAX_DECODE_DIMENSION));
    let width = u32::try_from((u64::from(vw) * target_edge).div_ceil(source_edge)).ok()?;
    let height = u32::try_from((u64::from(vh) * target_edge).div_ceil(source_edge)).ok()?;

    if canvas.width() != width {
        canvas.set_width(width);
    }
    if canvas.height() != height {
        canvas.set_height(height);
    }
    ctx.draw_image_with_html_video_element_and_dw_and_dh(
        video,
        0.0,
        0.0,
        f64::from(width),
        f64::from(height),
    )
    .ok()?;

    let rgba = ctx
        .get_image_data(0.0, 0.0, f64::from(width), f64::from(height))
        .ok()?
        .data();

    qr_scanner::decode_rgba(
        &rgba,
        usize::try_from(width).ok()?,
        usize::try_from(height).ok()?,
    )
}

#[cfg(target_arch = "wasm32")]
fn stop_media_stream(stream: &MediaStream) {
    let tracks = stream.get_tracks();
    for i in 0..tracks.length() {
        if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
            track.stop();
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn stop_stream_tracks(stream: Signal<Option<MediaStream>>) {
    if let Some(s) = &*stream.read() {
        stop_media_stream(s);
    }
}

fn focus_mounted_button(cancel_btn: Signal<Option<Rc<MountedData>>>) {
    if let Some(btn) = cancel_btn.read().clone() {
        spawn(async move {
            let _ = btn.set_focus(true).await;
        });
    }
}

fn make_cancel_key(
    cancel_btn: Signal<Option<Rc<MountedData>>>,
    mut cancel: impl FnMut() + Copy + 'static,
) -> impl FnMut(KeyboardEvent) + Copy + 'static {
    move |e: KeyboardEvent| {
        if e.key() == Key::Named(NamedKey::Tab) {
            e.prevent_default();
            focus_mounted_button(cancel_btn);
        } else if e.key() == Key::Named(NamedKey::Escape) {
            cancel();
        }
    }
}
/// Fullscreen camera overlay that scans for QR codes.
///
/// Calls `on_result(Some(decoded_string))` on success, or `on_result(None)` on
/// cancel / camera error.
#[component]
pub fn QrScannerOverlay(on_result: EventHandler<Option<String>>) -> Element {
    let cancel_btn: Signal<Option<Rc<MountedData>>> = use_signal(|| None);

    #[cfg(target_arch = "wasm32")]
    let return_focus = use_hook(|| {
        get_document()
            .and_then(|doc| doc.active_element())
            .and_then(|el| el.dyn_into::<HtmlElement>().ok())
    });
    #[cfg(target_arch = "wasm32")]
    use_drop(move || {
        if let Some(el) = return_focus.as_ref() {
            let _ = el.focus();
        }
    });

    // On native targets, delegate to the existing FFI scanner immediately.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut done = use_signal(|| false);

        use_resource(move || async move {
            let result = qr_scanner::scan_async().await;
            if !*done.peek() {
                done.set(true);
                let value = if result.is_empty() {
                    None
                } else {
                    Some(result)
                };
                on_result.call(value);
            }
        });

        // Stop the platform scan whenever the overlay goes away — cancel,
        // Escape, or parent unmount. Without this the camera keeps running
        // until the scan times out (Linux/Windows headless loops) or the
        // native preview lingers (macOS). Idempotent, so the extra call
        // after a normal finish is harmless.
        use_drop(qr_scanner::cancel_scan);

        let mut cancel = move || {
            if !*done.peek() {
                done.set(true);
                qr_scanner::cancel_scan();
                on_result.call(None);
            }
        };
        let cancel_click = move |_| cancel();
        let cancel_key = make_cancel_key(cancel_btn, cancel);

        return render_native_overlay(cancel_btn, cancel_click, cancel_key);
    }

    // -----------------------------------------------------------------------
    // WASM: live camera via getUserMedia
    // -----------------------------------------------------------------------
    #[cfg(target_arch = "wasm32")]
    {
        // Session state.
        let stream: Signal<Option<MediaStream>> = use_signal(|| None);
        let mut error = use_signal(|| None::<String>);
        let mut done = use_signal(|| false);
        let stream_ready = use_signal(|| Rc::new(Notify::new()));

        // Camera/session lifecycle: acquire stream on mount.
        //
        // Deliberately a *detached* task, not `use_resource`: Dioxus drops
        // resource futures at unmount, so cancelling while `getUserMedia`'s
        // permission prompt is pending would leak the stream the browser
        // hands back later (camera stays on). This task always runs to
        // completion and stops a late stream itself. The fallible
        // `try_peek` treats a dropped signal (component unmounted) the
        // same as `done == true`: stop the stream instead of storing it.
        {
            let mut stream_w = stream;
            use_hook(move || {
                spawn_local(async move {
                    let acquired = request_camera_stream().await;
                    let gone = done.try_peek().map_or(true, |d| *d);
                    if gone {
                        // `gone` is either a dropped signal (unmounted) or
                        // `done` already set. The decode loop went with the
                        // scope or exits on its own `done` check, so there is
                        // nothing to wake — and peeking a dropped
                        // `stream_ready` panics.
                        if let Some(s) = acquired.as_ref() {
                            stop_media_stream(s);
                        }
                        return;
                    }
                    match acquired {
                        Some(s) => stream_w.set(Some(s)),
                        None => error.set(Some("Camera not available".to_string())),
                    }
                    stream_ready.peek().notify_waiters();
                });
            });
        }

        // Camera/session lifecycle: attach stream to the video element.
        let stream_for_effect = stream;
        use_effect(move || {
            let Some(s) = &*stream_for_effect.read() else {
                return;
            };
            attach_stream_to_video(s);
        });

        // Decode loop: poll frames and decode QR payload.
        let stream_for_decode = stream;
        use_future(move || async move {
            let notify = stream_ready.peek().clone();
            let notified = notify.notified();
            if stream_for_decode.peek().is_none() && !done() {
                notified.await;
            }
            if done() || stream_for_decode.peek().is_none() {
                return;
            }

            // Prefer the browser's native BarcodeDetector (decodes straight
            // off the <video> element, no canvas readback); fall back to
            // canvas + rqrr where the engine doesn't ship one.
            let native = qr_scanner::native_qr_detector();
            let canvas_ctx = if native.is_none() {
                canvas_context()
            } else {
                None
            };
            if native.is_none() && canvas_ctx.is_none() {
                stop_stream_tracks(stream_for_decode);
                error.set(Some(
                    "QR scanning is unavailable in this browser.".to_string(),
                ));
                return;
            }

            loop {
                if done() {
                    break;
                }
                sleep_ms(DECODE_INTERVAL_MS).await;
                if done() {
                    break;
                }

                let Some(video) = video_element() else {
                    continue;
                };
                // A backgrounded tab shows no frames to the user, so skip the
                // decode work; the camera stream itself is left running.
                if page_hidden() {
                    continue;
                }
                let content = if let Some(detector) = &native {
                    detector.detect_qr(video.as_ref()).await
                } else if let Some((canvas, ctx)) = &canvas_ctx {
                    decode_from_video_frame(&video, canvas, ctx)
                } else {
                    None
                };

                if let Some(content) = content {
                    if !done() {
                        done.set(true);
                        stop_stream_tracks(stream_for_decode);
                        on_result.call(Some(content));
                    }
                    break;
                }
            }
        });

        // Camera/session lifecycle: stop tracks on exit.
        let stop_tracks = move || stop_stream_tracks(stream);

        use_drop(move || {
            stop_tracks();
        });

        let mut cancel = move || {
            if !done() {
                done.set(true);
                stream_ready.peek().notify_waiters();
                stop_tracks();
                on_result.call(None);
            }
        };
        let cancel_click = move |_| cancel();
        let cancel_key = make_cancel_key(cancel_btn, cancel);

        if let Some(err) = error.read().as_ref() {
            return render_wasm_error_overlay(err, cancel_btn, cancel_click, cancel_key);
        }

        render_wasm_scanner_overlay(cancel_btn, cancel_click, cancel_key)
    }
}
