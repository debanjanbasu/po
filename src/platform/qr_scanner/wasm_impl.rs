use js_sys::{Array, Object, Promise, Reflect};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;
use web_sys::window;

use crate::config;

// ---------------------------------------------------------------------------
// Native decode: the browser's Shape Detection API (`BarcodeDetector`).
// Hardware-accelerated and consumes `<video>`/`ImageBitmap` sources directly
// (no canvas readback). Feature-detected at runtime — Safari/Firefox don't
// ship it, so callers fall back to the pure-Rust `rqrr` decoder.
// ---------------------------------------------------------------------------

#[wasm_bindgen]
extern "C" {
    type BarcodeDetector;

    #[wasm_bindgen(constructor, catch)]
    fn new(options: &JsValue) -> Result<BarcodeDetector, JsValue>;

    #[wasm_bindgen(method, catch)]
    fn detect(this: &BarcodeDetector, source: &JsValue) -> Result<Promise, JsValue>;
}

/// QR detector backed by the browser's native `BarcodeDetector`.
pub struct NativeQrDetector(BarcodeDetector);

/// Returns the native detector, or `None` when the engine doesn't support it.
pub fn native_qr_detector() -> Option<NativeQrDetector> {
    let window = window()?;
    if !Reflect::has(&window, &JsValue::from_str("BarcodeDetector")).unwrap_or(false) {
        return None;
    }
    let formats = Array::of1(&JsValue::from_str("qr_code"));
    let options = Object::new();
    Reflect::set(&options, &JsValue::from_str("formats"), &formats).ok()?;
    BarcodeDetector::new(&options).ok().map(NativeQrDetector)
}

impl NativeQrDetector {
    /// Runs detection over any `ImageBitmapSource` (`<video>`, `ImageBitmap`,
    /// canvas). Returns the first non-empty QR payload; `None` on no match or
    /// error (e.g. the video has no frames yet).
    pub async fn detect_qr(&self, source: &JsValue) -> Option<String> {
        let promise = self.0.detect(source).ok()?;
        let results = JsFuture::from(promise).await.ok()?;
        let items: Array = results.dyn_into().ok()?;
        items.iter().find_map(|item| {
            Reflect::get(&item, &JsValue::from_str("rawValue"))
                .ok()
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty())
        })
    }
}

/// `true` when the browser exposes at least one camera.
///
/// `navigator.media_devices()` only proves the API exists — it is present in
/// every secure-origin browser, camera or not — so enumerate the devices for
/// a `videoinput` before offering the scan control.
pub async fn has_video_input() -> bool {
    let Some(devices) = window()
        .and_then(|w| w.navigator().media_devices().ok())
        .and_then(|media| media.enumerate_devices().ok())
        .map(JsFuture::from)
    else {
        return false;
    };
    let Ok(devices) = devices.await else {
        return false;
    };
    devices.dyn_into::<Array>().is_ok_and(|devices| {
        devices.iter().any(|device| {
            Reflect::get(&device, &JsValue::from_str("kind"))
                .ok()
                .and_then(|kind| kind.as_string())
                .is_some_and(|kind| kind == "videoinput")
        })
    })
}

pub fn decode_rgba(rgba: &[u8], w: usize, h: usize) -> Option<String> {
    let pixel_count = w.checked_mul(h)?;
    let byte_count = pixel_count.checked_mul(4)?;
    let pixels = rgba.get(..byte_count)?;
    super::decode_qr_from_fn(w, h, |x, y| {
        let idx = (y * w + x) * 4;
        ((u16::from(pixels[idx]) * config::qr::LUMA_R
            + u16::from(pixels[idx + 1]) * config::qr::LUMA_G
            + u16::from(pixels[idx + 2]) * config::qr::LUMA_B)
            >> 8) as u8
    })
}
