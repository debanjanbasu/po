//! QR code scanner bridge.
//!
//! Platform implementations are split by target:
//! - iOS/macOS/Android: native FFI plugins
//! - Linux: V4L2 camera capture + `rqrr`
//! - Windows: `MediaCapture` + `rqrr`
//! - WASM: camera stream + browser QR detection/canvas decode
//! - Other native targets: explicit compile failure

use crate::helpers::spawn_blocking;
#[cfg(not(target_arch = "wasm32"))]
use crate::platform::native_call;
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
use crate::{platform::with_autorelease_pool, storage::background_errors::report_nonfatal};
use bytes::Bytes;
use image::load_from_memory;
use rqrr::PreparedImage;
#[cfg(target_os = "linux")]
mod linux_impl;
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
mod mobile_impl;
#[cfg(target_arch = "wasm32")]
mod wasm_impl;
#[cfg(target_os = "windows")]
mod windows_impl;

#[cfg(target_os = "linux")]
use linux_impl::{plugin_can_scan, plugin_scan};
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
use mobile_impl::{plugin_can_scan, plugin_scan};
#[cfg(target_os = "windows")]
use windows_impl::{plugin_can_scan, plugin_scan};

#[cfg(not(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "android",
    target_os = "linux",
    target_os = "windows",
    target_arch = "wasm32",
)))]
compile_error!(
    "Unsupported target for QR scanner: implement a platform plugin module before building."
);

/// Cooperative cancellation for the headless capture loops (Linux/Windows),
/// which otherwise keep the camera running until the scan times out.
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod cancel_flag {
    use std::sync::atomic::{AtomicBool, Ordering};

    static CANCELLED: AtomicBool = AtomicBool::new(false);

    /// Cleared at scan-request time (not inside the blocking task) so a
    /// cancel issued while the task is still queued isn't wiped out.
    pub(super) fn reset() {
        CANCELLED.store(false, Ordering::Relaxed);
    }

    pub(super) fn request() {
        CANCELLED.store(true, Ordering::Relaxed);
    }

    pub(super) fn requested() -> bool {
        CANCELLED.load(Ordering::Relaxed)
    }
}

/// Requests cancellation of any in-flight native scan, releasing the camera.
///
/// - Linux/Windows: the headless capture loop polls a flag and exits.
/// - iOS/macOS: dismisses the native scanner UI (`VisionKit` / `AVFoundation`).
/// - Android: no-op — the Google Code Scanner system activity carries its
///   own cancel affordance and offers no programmatic cancel.
///
/// Idempotent and safe to call with no scan in flight.
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
pub fn cancel_scan() {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    cancel_flag::request();
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    {
        if let Ok(plugin) = mobile_impl::new_plugin() {
            mobile_impl::plugin_cancel_scan(&plugin);
        }
    }
}

#[cfg(target_os = "android")]
pub const fn cancel_scan() {}

/// Returns `true` if the device has a camera capable of QR scanning.
///
/// Native-only: the check is synchronous here, and the browser's is not (see
/// [`can_scan_async`]).
#[cfg(not(target_arch = "wasm32"))]
pub fn can_scan() -> bool {
    #[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
    {
        mobile_impl::new_plugin().is_ok_and(|plugin| plugin_can_scan(&plugin))
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        plugin_can_scan()
    }
}

/// Async camera availability probe — runs the native hardware/FFI check on
/// the blocking pool so component rendering never stalls the UI thread.
pub async fn can_scan_async() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        wasm_impl::has_video_input().await
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        spawn_blocking(can_scan).await.unwrap_or(false)
    }
}

/// Async QR scan.
#[cfg(not(target_arch = "wasm32"))]
pub async fn scan_async() -> String {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    cancel_flag::reset();
    let scanned = spawn_blocking(|| {
        #[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
        {
            let Ok(plugin) = mobile_impl::new_plugin() else {
                report_nonfatal(
                    "helpers.platform.qr_scanner",
                    "QR scanner plugin unavailable",
                    "plugin init failed",
                );
                return String::new();
            };
            // See `with_autorelease_pool` for why the pool is here and not
            // upstream in the fork's Swift codegen.
            with_autorelease_pool(|| plugin_scan(&plugin))
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            plugin_scan()
        }
    })
    .await;
    native_call(scanned, "helpers.platform.qr_scanner", "QR scan failed").unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
pub use wasm_impl::{decode_rgba, native_qr_detector};

/// Async wrapper around [`decode_image_bytes`]: image decode + QR detection
/// of a multi-megapixel photo takes long enough to stall the UI executor,
/// so it runs on the blocking pool (inline fallback on WASM).
pub async fn decode_image_bytes_async(data: Bytes) -> Option<String> {
    spawn_blocking(move || decode_image_bytes(&data))
        .await
        .ok()
        .flatten()
}

/// Decode a QR code from PNG, JPEG, or WebP image bytes.
/// Uses the `image` crate for auto-detection and decoding.
pub fn decode_image_bytes(data: &[u8]) -> Option<String> {
    let img = load_from_memory(data).ok()?;
    let grey = img.to_luma8();
    let (w, h) = grey.dimensions();
    decode_luma_qr(grey.as_raw(), w as usize, h as usize)
}

/// Decodes a QR code by sampling 8-bit luma values directly from a closure.
pub(super) fn decode_qr_from_fn(
    width: usize,
    height: usize,
    fill: impl FnMut(usize, usize) -> u8,
) -> Option<String> {
    if width == 0 || height == 0 {
        return None;
    }
    let mut prepared = PreparedImage::prepare_from_greyscale(width, height, fill);
    prepared
        .detect_grids()
        .into_iter()
        .find_map(|g| g.decode().ok().map(|(_, content)| content))
}

/// Decodes a QR code from tightly-packed 8-bit luma data.
///
/// The shared tail of every platform decoder (Linux V4L2, Windows
/// `MediaCapture`, WASM canvas readback, in-memory image bytes).
pub(super) fn decode_luma_qr(luma: &[u8], width: usize, height: usize) -> Option<String> {
    if width.checked_mul(height)? > luma.len() {
        return None;
    }
    decode_qr_from_fn(width, height, |x, y| luma[y * width + x])
}
