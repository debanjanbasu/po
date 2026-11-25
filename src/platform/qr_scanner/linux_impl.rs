use std::{
    thread,
    time::{Duration, Instant},
};

use crate::config;

use v4l::{Format, FourCC, buffer::Type, io::traits::CaptureStream, prelude::*, video::Capture};

pub(super) fn plugin_can_scan() -> bool {
    Device::new(0).is_ok_and(|dev| negotiate_format(&dev).is_some())
}

pub(super) fn plugin_scan() -> String {
    scan_loop().unwrap_or_default()
}

/// Pixel formats we can turn into 8-bit luma for `rqrr`.
#[derive(Clone, Copy)]
enum LumaSource {
    /// 8-bit greyscale: rows are luma already.
    Grey,
    /// Packed 4:2:2 (`[Y0, U, Y1, V]`): every even byte is luma.
    Yuyv,
}

/// Negotiates a capture format we can actually decode.
///
/// V4L2's `S_FMT` is a request — drivers substitute their closest supported
/// format — so the returned format must be re-checked, never assumed. Formats
/// we don't understand (MJPG, NV12, …) are rejected rather than misread as
/// luma bytes.
fn negotiate_format(dev: &Device) -> Option<(Format, LumaSource)> {
    for (code, source) in [(b"GREY", LumaSource::Grey), (b"YUYV", LumaSource::Yuyv)] {
        let requested = FourCC::new(code);
        let Ok(actual) = dev.set_format(&Format::new(640, 480, requested)) else {
            continue;
        };
        if actual.fourcc == requested && actual.width > 0 && actual.height > 0 {
            return Some((actual, source));
        }
    }
    None
}

fn scan_loop() -> Option<String> {
    let dev = Device::new(0).ok()?;
    let (fmt, source) = negotiate_format(&dev)?;

    let width = usize::try_from(fmt.width).ok()?;
    let height = usize::try_from(fmt.height).ok()?;
    let stride = usize::try_from(fmt.stride).ok()?;

    let mut stream = MmapStream::with_buffers(&dev, Type::VideoCapture, 4).ok()?;

    let deadline = Instant::now() + Duration::from_secs(config::qr::SCAN_TIMEOUT_SECS);
    while Instant::now() < deadline && !super::cancel_flag::requested() {
        match stream.next() {
            Ok((buf, _meta)) => {
                if let Some(code) = decode_frame(buf, source, width, height, stride) {
                    return Some(code);
                }
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    None
}

/// Decodes 8-bit luma directly from a captured frame, honoring the
/// driver-reported row stride (rows may be padded). Returns `None` when the
/// buffer is too short for the negotiated geometry — skip the frame rather
/// than decode garbage.
fn decode_frame(
    buf: &[u8],
    source: LumaSource,
    width: usize,
    height: usize,
    stride: usize,
) -> Option<String> {
    let row_bytes = match source {
        LumaSource::Grey => width,
        LumaSource::Yuyv => width.checked_mul(2)?,
    };
    // Some drivers report stride 0; fall back to tightly packed rows.
    let stride = stride.max(row_bytes);
    let needed = height
        .checked_sub(1)?
        .checked_mul(stride)?
        .checked_add(row_bytes)?;
    if buf.len() < needed {
        return None;
    }
    super::decode_qr_from_fn(width, height, |x, y| match source {
        LumaSource::Grey => buf[y * stride + x],
        LumaSource::Yuyv => buf[y * stride + x * 2],
    })
}
