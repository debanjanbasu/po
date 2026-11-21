//! QR code rendering: generates WebP bytes from arbitrary data for `QrShareCard`.

use std::io::Cursor;

use anyhow::Result;
use bytes::Bytes;
use image::{ExtendedColorType, Luma, codecs::webp::WebPEncoder};
use qrcode::{QrCode, render::Renderer};

use crate::config::e2e;
#[cfg(feature = "mobile")]
use crate::platform::qr_scanner;

/// Renders `link` into WebP. The QR encodes the canonical https share
/// URL so camera apps (which open the URL) and in-app scanners (which
/// parse it) behave identically.
///
/// Must be called from an async context — the heavy rendering is
/// offloaded via `spawn_blocking`.
pub fn generate_qr_webp(link: &str) -> Result<Bytes> {
    Ok(Bytes::from(render_webp(link, 16)?))
}

/// Harness seam (no-op unless `PO_E2E=1` in a debug build): decodes the QR we
/// just rendered with the app's own scanner and reports whether the payload
/// round-tripped exactly. A lossy encode or a mismatched URL fails here instead
/// of silently shipping. Emits `event=qr` or `event=qr_decode_failed`.
#[allow(clippy::unused_async)] // async at the seam (mobile impl awaits)
pub async fn report_qr_round_trip(
    #[cfg_attr(not(feature = "mobile"), allow(unused_variables))] webp: &Bytes,
    #[cfg_attr(not(feature = "mobile"), allow(unused_variables))] link: &str,
) {
    if !e2e::enabled() {}
    #[cfg(feature = "mobile")]
    match qr_scanner::decode_image_bytes_async(webp.clone()).await {
        Some(decoded) if decoded == link => e2e::event!("qr", link = %link),
        Some(_) => e2e::event!("qr_decode_failed", reason = "mismatch"),
        None => e2e::event!("qr_decode_failed", reason = "undecodable"),
    }
}

/// Renders `data` as a QR code WebP (lossless, 8-bit grayscale, scaled up).
/// Returns raw WebP bytes suitable for download/sharing/display.
///
fn render_webp(data: &str, scale: u32) -> Result<Vec<u8>> {
    let code = QrCode::new(data.as_bytes())?;
    let modules = code.to_colors();
    let image = Renderer::<Luma<u8>>::new(&modules, code.width(), 4)
        .module_dimensions(scale, scale)
        .build();
    let (img_width, img_height) = image.dimensions();

    let mut buf = Vec::new();
    WebPEncoder::new_lossless(Cursor::new(&mut buf)).encode(
        &image.into_raw(),
        img_width,
        img_height,
        ExtendedColorType::L8,
    )?;
    Ok(buf)
}
