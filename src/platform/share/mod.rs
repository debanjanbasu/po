//! Platform-aware sharing utilities (Web Share API, clipboard, download).

// The Rust idents below are the native entry points: `manganis` camel-cases
// them for Kotlin, so `share_text` resolves the platform's `shareText`.
// Renaming one renames the symbol the native side looks up. (The Swift
// declarations live in `platform::apple_ffi`.)

#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
#[cfg(target_os = "ios")]
use anyhow::{Result as AnyhowResult, anyhow, bail, ensure};
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
use base64::engine::{Engine, general_purpose::STANDARD};
#[cfg(target_os = "linux")]
use std::env;
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
use std::fmt::Debug;
#[cfg(target_os = "ios")]
use std::fs::{metadata, read, remove_file};
#[cfg(target_arch = "wasm32")]
use std::future::ready;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::{
    io::Write,
    process::{Command, Stdio},
};
use thiserror::Error;

#[cfg(not(target_arch = "wasm32"))]
use crate::helpers::spawn_blocking;
#[cfg(target_os = "ios")]
use crate::platform::apple_ffi::pick_file;
#[cfg(any(target_os = "ios", target_os = "macos"))]
use crate::platform::apple_ffi::{SharePlugin, copy_text, share_file, share_text};
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
use crate::platform::with_autorelease_pool;
#[cfg(not(target_arch = "wasm32"))]
use crate::storage::background_errors::report_nonfatal;
#[cfg(target_arch = "wasm32")]
mod wasm_impl;

#[cfg(target_arch = "wasm32")]
pub use wasm_impl::{copy_to_clipboard, download_bytes, share_text_and_image};

/// Expected capability/permission failures remain distinguishable from an
/// unexpected failure. Messages are safe to display beside the active control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ShareError {
    #[error("This action is not supported on this device or browser.")]
    Unsupported,
    #[error("Action cancelled.")]
    Cancelled,
    #[cfg(any(
        target_arch = "wasm32",
        target_os = "ios",
        target_os = "macos",
        target_os = "android"
    ))]
    #[error("Permission was denied. Allow access and try again.")]
    Denied,
    #[error("The action could not be completed. Please try again.")]
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareResult {
    #[cfg(target_arch = "wasm32")]
    Shared,
    #[cfg(target_arch = "wasm32")]
    TextOnly,
    #[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
    Presented,
    ClipboardFallback,
}

impl ShareResult {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            #[cfg(target_arch = "wasm32")]
            Self::Shared => "Sharing request completed.",
            #[cfg(target_arch = "wasm32")]
            Self::TextOnly => "Shared as text. Save the QR image separately if needed.",
            #[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
            Self::Presented => "Share dialog opened. Complete sharing there.",
            Self::ClipboardFallback => {
                "Sharing unavailable here. Link copied to clipboard instead."
            }
        }
    }
}

/// A download request or native dialog is not proof that the user saved a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadResult {
    #[cfg(target_arch = "wasm32")]
    Triggered,
    #[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
    Presented,
}

impl DownloadResult {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            #[cfg(target_arch = "wasm32")]
            Self::Triggered => "Download requested. Check your browser's downloads.",
            #[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
            Self::Presented => "Export dialog opened. Complete the save or share there.",
        }
    }
}

#[cfg(target_os = "android")]
mod android_impl {
    use crate::platform::android_plugin::load_activity_plugin;

    #[manganis::ffi("native/android/share")]
    extern "Kotlin" {
        pub type SharePlugin;
        pub fn share_text(this: &SharePlugin, input: String) -> String;
        pub fn copy_text(this: &SharePlugin, text: String) -> String;
        pub fn share_file(this: &SharePlugin, input: String) -> String;
    }

    pub fn new_plugin() -> Result<SharePlugin, String> {
        load_activity_plugin("com.debanjanbasu.po.SharePlugin", "SharePlugin")
            .map(SharePlugin::from_global_ref)
    }
}

#[cfg(target_os = "android")]
use android_impl::{SharePlugin, copy_text, share_file, share_text};
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
fn init_share_plugin(context: &'static str) -> Result<SharePlugin, ShareError> {
    let plugin_result = {
        #[cfg(target_os = "android")]
        {
            android_impl::new_plugin()
        }
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        {
            SharePlugin::new()
        }
    };
    plugin_result.map_err(|error| {
        report_nonfatal("platform.share", context, format!("{error:?}"));
        ShareError::Unsupported
    })
}

#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
fn status_result(status: &str, context: &'static str) -> Result<(), ShareError> {
    match status.to_ascii_lowercase().as_str() {
        "true" => Ok(()),
        "unsupported" => Err(ShareError::Unsupported),
        "cancelled" => Err(ShareError::Cancelled),
        "denied" => Err(ShareError::Denied),
        "false" | "failed" => {
            report_nonfatal("platform.share", context, "platform reported failure");
            Err(ShareError::Failed)
        }
        // A native side that could not start its work in time (see
        // `SharePlugin.runOnMainThreadSync`) cancels it instead of reporting a
        // failure for something that then happens anyway.
        "error:timeout" => {
            report_nonfatal("platform.share", context, "native call timed out");
            Err(ShareError::Failed)
        }
        _ => {
            report_nonfatal("platform.share", context, "invalid platform response");
            Err(ShareError::Failed)
        }
    }
}

#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
fn native_result<E: Debug>(
    result: Result<String, E>,
    context: &'static str,
) -> Result<(), ShareError> {
    let status = result.map_err(|error| {
        report_nonfatal("platform.share", context, format!("{error:?}"));
        ShareError::Failed
    })?;
    status_result(&status, context)
}

/// Copies text only when the native clipboard confirms completion.
///
/// Runs on the blocking pool on every platform: the native call marshals to the
/// main thread and waits on a semaphore, which must not park an async worker.
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
pub async fn copy_to_clipboard(text: &str) -> Result<(), ShareError> {
    let owned = text.to_owned();
    spawn_blocking(move || {
        // See `with_autorelease_pool` for why the pool is here and not upstream.
        with_autorelease_pool(|| {
            let plugin = init_share_plugin("Native clipboard plugin unavailable")?;
            native_result(copy_text(&plugin, owned), "Native clipboard copy failed")
        })
    })
    .await
    .unwrap_or_else(|error| {
        report_nonfatal("platform.share", "Native clipboard worker failed", error);
        Err(ShareError::Failed)
    })
}

/// Presents the native iOS document picker and reads the selected backup file.
#[cfg(target_os = "ios")]
pub async fn pick_backup_file(max_bytes: u64) -> AnyhowResult<Option<Vec<u8>>> {
    spawn_blocking(move || {
        let plugin = SharePlugin::new()
            .map_err(|_| anyhow!("Could not open the iOS backup picker. Please try again."))?;
        // See `with_autorelease_pool` for why the pool is here and not upstream.
        let path = with_autorelease_pool(|| pick_file(&plugin))
            .map_err(|_| anyhow!("Could not open the iOS backup picker. Please try again."))?;
        if path.is_empty() {
            return Ok(None);
        }
        if path.starts_with("error:") {
            bail!("Could not open the iOS backup picker. Please try again.");
        }
        let result = (|| {
            let meta = metadata(&path).map_err(|_| {
                anyhow!("Could not read the selected backup file. Please try again.")
            })?;
            ensure!(
                meta.len() <= max_bytes,
                "Selected file is larger than the maximum allowed size of {max_bytes} bytes."
            );
            read(&path)
                .map_err(|_| anyhow!("Could not read the selected backup file. Please try again."))
        })();
        let _ = remove_file(&path);
        result.map(Some)
    })
    .await
    .map_err(|_| anyhow!("Could not open the iOS backup picker. Please try again."))?
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn pipe_to_command(program: &str, args: &[&str], bytes: &[u8]) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote_ok = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(bytes).is_ok());
    let status_ok = child.wait().is_ok_and(|status| status.success());
    wrote_ok && status_ok
}

/// Copies text to the OS clipboard on desktop (Linux/Windows).
#[cfg(all(
    not(target_arch = "wasm32"),
    not(any(target_os = "ios", target_os = "macos", target_os = "android"))
))]
pub async fn copy_to_clipboard(text: &str) -> Result<(), ShareError> {
    let owned = text.to_owned();
    spawn_blocking(move || {
        let bytes = owned.as_bytes();
        #[cfg(target_os = "linux")]
        let copied = if env::var_os("WAYLAND_DISPLAY").is_some() {
            // Wayland session: `wl-copy` owns the clipboard there.
            pipe_to_command("wl-copy", &[], bytes)
                || pipe_to_command("xclip", &["-selection", "clipboard"], bytes)
                || pipe_to_command("xsel", &["--clipboard", "--input"], bytes)
        } else {
            // X11 session: `wl-copy` daemonizes and can report success without
            // a compositor, so prefer the X11 tools and keep `wl-copy` last.
            pipe_to_command("xclip", &["-selection", "clipboard"], bytes)
                || pipe_to_command("xsel", &["--clipboard", "--input"], bytes)
                || pipe_to_command("wl-copy", &[], bytes)
        };
        #[cfg(target_os = "windows")]
        let copied = pipe_to_command("clip", &[], bytes);
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        let copied = {
            let _ = bytes;
            false
        };
        if copied {
            Ok(())
        } else {
            report_nonfatal(
                "platform.share",
                "Desktop clipboard copy failed",
                "no clipboard utility succeeded",
            );
            Err(ShareError::Failed)
        }
    })
    .await
    .unwrap_or_else(|error| {
        report_nonfatal("platform.share", "Desktop clipboard worker failed", error);
        Err(ShareError::Failed)
    })
}

#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
fn download_bytes_sync(
    bytes: &[u8],
    filename: &str,
    mime: &str,
) -> Result<DownloadResult, ShareError> {
    // The whole file travels as one base64 string, the largest this crate ever
    // hands the native side — see `with_autorelease_pool` for why the pool is
    // here and not upstream.
    with_autorelease_pool(|| {
        let plugin = init_share_plugin("Native share plugin unavailable for file export")?;
        // One JSON document via serde_json: the base64 body is the only large
        // part, and `json!` keeps the key order and escaping identical to the
        // hand-rolled assembly it replaces.
        let payload = serde_json::json!({
            "filename": filename,
            "data": STANDARD.encode(bytes),
            "mime": mime,
        })
        .to_string();
        native_result(share_file(&plugin, payload), "Native file share failed")?;
        Ok(DownloadResult::Presented)
    })
}

/// Async file download/export: offloads native save/share dialogs to a
/// blocking worker instead of `block_in_place` on the UI thread, and invokes
/// `download_bytes` on `wasm32`.
pub async fn download_bytes_async(
    bytes: Vec<u8>,
    filename: &str,
    mime: &str,
) -> Result<DownloadResult, ShareError> {
    #[cfg(target_arch = "wasm32")]
    {
        // The browser download is synchronous and reports no completion, so
        // the result itself is what gets awaited. The `async` stays: callers
        // await this uniformly, and `clippy::unused_async` requires an await.
        ready(download_bytes(&bytes, filename, mime)).await
    }
    #[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
    {
        let filename = filename.to_owned();
        let mime = mime.to_owned();
        spawn_blocking(move || download_bytes_sync(&bytes, &filename, &mime))
            .await
            .unwrap_or_else(|error| {
                report_nonfatal("platform.share", "Native file export worker failed", error);
                Err(ShareError::Failed)
            })
    }
    #[cfg(all(
        not(target_arch = "wasm32"),
        not(any(target_os = "ios", target_os = "macos", target_os = "android"))
    ))]
    {
        let _ = (bytes, filename, mime);
        spawn_blocking(|| Err(ShareError::Unsupported))
            .await
            .unwrap_or(Err(ShareError::Failed))
    }
}

/// Shares text + image via the platform share sheet.
///
/// Native success confirms presentation only; cancellation after presentation
/// cannot be observed through the synchronous string bridge.
#[cfg(any(target_os = "ios", target_os = "macos", target_os = "android"))]
pub async fn share_text_and_image(
    text: &str,
    image_bytes: &[u8],
) -> Result<ShareResult, ShareError> {
    let text_owned = text.to_owned();
    let image_bytes = image_bytes.to_vec();
    let share_outcome = spawn_blocking(move || {
        // The QR image travels as base64 — see `with_autorelease_pool` for why
        // the pool is here and not upstream.
        with_autorelease_pool(|| {
            let plugin = init_share_plugin("Native share plugin unavailable")?;
            let payload = serde_json::json!({
                "text": text_owned,
                "image": STANDARD.encode(&image_bytes),
            })
            .to_string();
            native_result(share_text(&plugin, payload), "Native share failed")
        })
    })
    .await
    .unwrap_or_else(|error| {
        report_nonfatal("platform.share", "Native share worker failed", error);
        Err(ShareError::Failed)
    });
    match share_outcome {
        Ok(()) => Ok(ShareResult::Presented),
        Err(ShareError::Unsupported) => copy_to_clipboard(text)
            .await
            .map(|()| ShareResult::ClipboardFallback),
        Err(error) => Err(error),
    }
}

#[cfg(all(
    not(target_arch = "wasm32"),
    not(any(target_os = "ios", target_os = "macos", target_os = "android"))
))]
pub async fn share_text_and_image(
    text: &str,
    _image_bytes: &[u8],
) -> Result<ShareResult, ShareError> {
    copy_to_clipboard(text)
        .await
        .map(|()| ShareResult::ClipboardFallback)
}
