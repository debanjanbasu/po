use crate::{platform::native_call, storage::background_errors::report_nonfatal};
use std::fmt::Display;

#[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
#[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
use crate::platform::apple_ffi;

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
    use jni::objects::{Global, JObject};

    use crate::platform::android_plugin::{load_activity_plugin, with_activity};

    // Hand-written JNI bridge to `com.debanjanbasu.po.QRScannerPlugin` (the
    // dioxus-era `manganis::ffi` Kotlin codegen died with that pipeline).
    pub struct QRScannerPlugin {
        instance: Global<JObject<'static>>,
    }

    impl QRScannerPlugin {
        /// Wraps a freshly loaded plugin instance.
        pub fn from_global_ref(instance: Global<JObject<'static>>) -> Self {
            Self { instance }
        }

        /// Calls a `(String) -> String` or `() -> String` Kotlin method.
        fn call_string(&self, method: &str, arg: Option<&str>) -> String {
            with_activity(|env, _activity| {
                Some(try {
                    let jarg = arg.map(|a| env.new_string(a)).transpose().ok().flatten();
                    let (signature, value) = match &jarg {
                        Some(jarg) => (
                            "(Ljava/lang/String;)Ljava/lang/String;",
                            Some(jni::objects::JValue::Object(jarg)),
                        ),
                        None => ("()Ljava/lang/String;", None),
                    };
                    let runtime_signature =
                        jni::signature::RuntimeMethodSignature::from_str(signature).ok()?;
                    let signature: jni::signature::MethodSignature =
                        jni::signature::MethodSignature::from(&runtime_signature);
                    let method = jni::strings::JNIString::new(method);
                    let result = match value {
                        Some(v) => env.call_method(&self.instance, &method, &signature, &[v]),
                        None => env.call_method(&self.instance, &method, &signature, &[]),
                    };
                    let string = result.and_then(|v| v.l()).ok()?;
                    let string = env.as_cast::<jni::objects::JString>(&string).ok()?;
                    string.try_to_string(env).ok()?
                })
            })
            .flatten()
            .unwrap_or_default()
        }

        /// Kotlin `canScan(): String`.
        pub fn can_scan(&self) -> String {
            self.call_string("canScan", None)
        }

        /// Kotlin `scan(): String`.
        pub fn scan(&self) -> String {
            self.call_string("scan", None)
        }
    }

    pub fn new_plugin() -> Result<QRScannerPlugin, String> {
        load_activity_plugin("com.debanjanbasu.po.QRScannerPlugin", "QRScannerPlugin")
            .map(QRScannerPlugin::from_global_ref)
    }
}

#[cfg(target_os = "android")]
pub(super) use android_impl::QRScannerPlugin;

pub(super) fn new_plugin() -> Result<QRScannerPlugin, String> {
    #[cfg(target_os = "android")]
    {
        android_impl::new_plugin()
    }
    #[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
    {
        QRScannerPlugin::new().map_err(String::from)
    }
}

pub(super) fn plugin_can_scan(plugin: &QRScannerPlugin) -> bool {
    parse_can_scan(Ok::<String, std::convert::Infallible>(plugin.can_scan()))
}

pub(super) fn plugin_scan(plugin: &QRScannerPlugin) -> String {
    native_call(
        Ok::<_, jni::errors::Error>(plugin.scan()),
        "platform.qr_scanner",
        "QR scan bridge failed",
    )
    .unwrap_or_default()
}

/// Dismisses an in-flight native scanner UI, releasing the camera.
///
/// Apple-only: on Android the Google Code Scanner is a system activity with
/// its own cancel affordance, and its `Task` API offers no programmatic
/// cancel.
#[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
pub(super) fn plugin_cancel_scan(plugin: &QRScannerPlugin) {
    if let Err(error) = cancel_scan(plugin) {
        report_nonfatal("platform.qr_scanner", "QR scan cancellation failed", error);
    }
}
