use crate::{platform::native_call, storage::background_errors::report_nonfatal};
use std::fmt::Display;

#[cfg(any(target_os = "ios", target_os = "macos"))]
use crate::platform::apple_ffi::{QRScannerPlugin, can_scan, cancel_scan, scan_sync as scan};

fn parse_can_scan<E: Display>(result: Result<String, E>) -> bool {
    match result {
        Ok(value) if value.eq_ignore_ascii_case("true") => true,
        Ok(value) if value.eq_ignore_ascii_case("false") => false,
        Ok(_) => {
            report_nonfatal(
                "platform.qr_scanner",
                "QR scanner availability check failed",
                "invalid native response",
            );
            false
        }
        Err(error) => {
            report_nonfatal(
                "platform.qr_scanner",
                "QR scanner availability check failed",
                error,
            );
            false
        }
    }
}

#[cfg(target_os = "android")]
mod android_impl {
    use crate::platform::android_plugin::load_activity_plugin;

    // The Rust idents are the native entry points: `manganis` camel-cases them
    // for Kotlin, so `can_scan` resolves `canScan`. Renaming one renames the
    // symbol the platform looks up.
    #[manganis::ffi("native/android/qrscanner")]
    extern "Kotlin" {
        pub type QRScannerPlugin;
        pub fn can_scan(this: &QRScannerPlugin) -> String;
        pub fn scan(this: &QRScannerPlugin) -> String;
    }

    pub fn new_plugin() -> Result<QRScannerPlugin, String> {
        load_activity_plugin("com.debanjanbasu.po.QRScannerPlugin", "QRScannerPlugin")
            .map(QRScannerPlugin::from_global_ref)
    }
}

#[cfg(target_os = "android")]
pub(super) use android_impl::QRScannerPlugin;
#[cfg(target_os = "android")]
use android_impl::{can_scan, scan};

pub(super) fn new_plugin() -> Result<QRScannerPlugin, String> {
    #[cfg(target_os = "android")]
    {
        android_impl::new_plugin()
    }
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    {
        QRScannerPlugin::new().map_err(String::from)
    }
}

pub(super) fn plugin_can_scan(plugin: &QRScannerPlugin) -> bool {
    parse_can_scan(can_scan(plugin))
}

pub(super) fn plugin_scan(plugin: &QRScannerPlugin) -> String {
    native_call(scan(plugin), "platform.qr_scanner", "QR scan bridge failed").unwrap_or_default()
}

/// Dismisses an in-flight native scanner UI, releasing the camera.
///
/// Apple-only: on Android the Google Code Scanner is a system activity with
/// its own cancel affordance, and its `Task` API offers no programmatic
/// cancel.
#[cfg(any(target_os = "ios", target_os = "macos"))]
pub(super) fn plugin_cancel_scan(plugin: &QRScannerPlugin) {
    if let Err(error) = cancel_scan(plugin) {
        report_nonfatal("platform.qr_scanner", "QR scan cancellation failed", error);
    }
}
