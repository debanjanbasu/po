use crate::{config, helpers::async_utils::sleep_ms, platform::block_on_winrt};
use std::time::{Duration, Instant};

use windows::{
    Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
    Media::Capture::{MediaCapture, MediaCaptureInitializationSettings, StreamingCaptureMode},
    Storage::Streams::{Buffer, DataReader},
};

async fn init_capture() -> Option<MediaCapture> {
    let settings = MediaCaptureInitializationSettings::new().ok()?;
    settings
        .SetStreamingCaptureMode(StreamingCaptureMode::Video)
        .ok()?;
    let capture = MediaCapture::new().ok()?;
    capture
        .InitializeWithSettingsAsync(&settings)
        .ok()?
        .await
        .ok()?;
    Some(capture)
}

pub(super) fn plugin_can_scan() -> bool {
    block_on_winrt(init_capture()).flatten().is_some()
}

pub(super) fn plugin_scan() -> String {
    block_on_winrt(async { scan_loop().await })
        .flatten()
        .unwrap_or_default()
}

async fn stop_preview(capture: &MediaCapture) {
    if let Ok(op) = capture.StopPreviewAsync() {
        let _ = op.await;
    }
}

async fn scan_loop() -> Option<String> {
    let capture = init_capture().await?;
    capture.StartPreviewAsync().ok()?.await.ok()?;

    let mut pixels = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(config::qr::SCAN_TIMEOUT_SECS);
    while Instant::now() < deadline && !super::cancel_flag::requested() {
        if let Some(code) = try_decode_frame(&capture, &mut pixels).await {
            stop_preview(&capture).await;
            return Some(code);
        }
        sleep_ms(250).await;
    }

    stop_preview(&capture).await;
    None
}

async fn try_decode_frame(capture: &MediaCapture, pixels: &mut Vec<u8>) -> Option<String> {
    let frame = capture.GetPreviewFrameAsync().ok()?.await.ok()?;
    let bitmap = frame.SoftwareBitmap().ok()?;
    let width = bitmap.PixelWidth().ok()? as u32;
    let height = bitmap.PixelHeight().ok()? as u32;

    let gray = if bitmap.BitmapPixelFormat().ok()? == BitmapPixelFormat::Gray8 {
        bitmap
    } else {
        SoftwareBitmap::Convert(&bitmap, BitmapPixelFormat::Gray8).ok()?
    };

    let pixel_count_u64 = u64::from(width).checked_mul(u64::from(height))?;
    let pixel_count = u32::try_from(pixel_count_u64).ok()?;
    let buffer = Buffer::Create(pixel_count).ok()?;
    gray.CopyToBuffer(&buffer).ok()?;

    let reader = DataReader::FromBuffer(&buffer).ok()?;
    let pixel_len = usize::try_from(pixel_count).ok()?;
    pixels.resize(pixel_len, 0);
    reader.ReadBytes(pixels).ok()?;

    super::decode_luma_qr(pixels, width as usize, height as usize)
}
