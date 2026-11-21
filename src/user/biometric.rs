//! Biometric authentication bridge.
//!
//! On iOS/macOS the apple bridge generates real FFI; on Android a
//! bindings to native `BiometricPlugin` classes. On Windows, we use the
//! native Windows Hello API via `windows-rs`.
//!
//! ## Platform behaviour differences
//!
//! - **iOS/macOS**: Uses `deviceOwnerAuthentication` which accepts biometrics
//!   **or** device passcode as fallback.
//! - **Android**: Uses `BIOMETRIC_STRONG` which requires enrolled biometrics
//!   (fingerprint/face) — no passcode fallback.
//! - **Windows**: Uses Windows Hello (`UserConsentVerifier`).
//! - **Web/Linux**: Always returns `false` (no biometric hardware access).

// Only `canCheck` in the Android `extern "Kotlin"` block needs this: the ident
// hand-written JNI bridge calls the Kotlin plugin. Swift/ObjC here IS the
// written on a declaration, so the module is the narrowest scope that reaches
// the expansion — same reason and wording as `platform::apple_ffi`.
#![allow(non_snake_case)]

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "android",
    target_os = "windows"
))]
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[cfg(target_os = "android")]
use crate::platform::android_plugin::load_activity_plugin;
#[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
#[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
use crate::platform::apple_ffi;
#[cfg(target_os = "windows")]
use crate::platform::block_on_winrt;
#[cfg(all(
    any(target_os = "ios", target_os = "macos", target_os = "android"),
    feature = "mobile"
))]
use crate::platform::with_autorelease_pool;
#[cfg(all(
    any(target_os = "ios", target_os = "macos", target_os = "android"),
    feature = "mobile"
))]
use crate::storage::background_errors::report_nonfatal;
use crate::{
    helpers::{recover_poison, spawn_blocking},
    platform::native_call,
};
#[cfg(target_os = "android")]
use jni::objects::{Global, JObject};
#[cfg(target_os = "windows")]
use windows::{
    Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    },
    core::HSTRING,
};

// Android – Kotlin via JNI (hand-written bridge; the dioxus-era
// `manganis::ffi` Kotlin codegen died with the dioxus pipeline).
#[cfg(target_os = "android")]
pub struct BiometricPlugin {
    instance: Global<JObject<'static>>,
}

#[cfg(target_os = "android")]
impl BiometricPlugin {
    /// Wraps a freshly loaded plugin instance.
    pub fn from_global_ref(instance: Global<JObject<'static>>) -> Self {
        Self { instance }
    }

    /// Calls a `() -> String` or `(String) -> String` Kotlin method.
    fn call_string(&self, method: &str, reason: Option<&str>) -> String {
        crate::platform::android_plugin::with_activity(|env, _activity| {
            Some(try {
                let jreason = reason.map(|r| env.new_string(r)).transpose().ok().flatten();
                let (signature, arg) = match &jreason {
                    Some(jreason) => (
                        "(Ljava/lang/String;)Ljava/lang/String;",
                        Some(jni::objects::JValue::Object(jreason)),
                    ),
                    None => ("()Ljava/lang/String;", None),
                };
                let runtime_signature =
                    jni::signature::RuntimeMethodSignature::from_str(signature).ok()?;
                let signature: jni::signature::MethodSignature =
                    jni::signature::MethodSignature::from(&runtime_signature);
                let method = jni::strings::JNIString::new(method);
                let result = match arg {
                    Some(value) => env.call_method(&self.instance, &method, &signature, &[value]),
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

    /// Kotlin `canCheck(): String`.
    pub fn can_check(&self) -> String {
        self.call_string("canCheck", None)
    }

    /// Kotlin `authenticate(reason: String): String`.
    pub fn authenticate(&self, reason: &str) -> String {
        self.call_string("authenticate", Some(reason))
    }
}
// Windows – Windows Hello via windows-rs.
#[cfg(target_os = "windows")]
fn can_check_impl() -> bool {
    let available = block_on_winrt(async {
        UserConsentVerifier::CheckAvailabilityAsync()
            .ok()?
            .await
            .ok()
    })
    .flatten();
    matches!(available, Some(UserConsentVerifierAvailability::Available))
}

#[cfg(target_os = "windows")]
fn authenticate_sync(reason: &str) -> bool {
    let result = block_on_winrt(async {
        UserConsentVerifier::RequestVerificationAsync(&HSTRING::from(reason))
            .ok()?
            .await
            .ok()
    })
    .flatten();
    matches!(result, Some(UserConsentVerificationResult::Verified))
}

#[cfg(all(
    any(target_os = "ios", target_os = "macos", target_os = "android"),
    feature = "mobile"
))]
fn new_plugin() -> Result<BiometricPlugin, String> {
    #[cfg(target_os = "android")]
    {
        load_activity_plugin("com.debanjanbasu.po.BiometricPlugin", "BiometricPlugin")
            .map(BiometricPlugin::from_global_ref)
    }
    #[cfg(all(any(target_os = "ios", target_os = "macos"), feature = "mobile"))]
    {
        BiometricPlugin::new().map_err(String::from)
    }
}

// Both wrappers below are the file's Rust→Swift boundary, so the pool goes
// around the bridge call itself: `can_check` also runs from a component render
// and `can_check_async` from the blocking pool, and only one of those threads
// has a pool. See `with_autorelease_pool` for why the pool is here and not
// upstream in the fork's `generate_ios_function`.
#[cfg(all(
    any(target_os = "ios", target_os = "macos", target_os = "android"),
    feature = "mobile"
))]
fn plugin_can_check(plugin: &BiometricPlugin) -> bool {
    native_call(
        with_autorelease_pool(|| Ok::<_, jni::errors::Error>(plugin.can_check())),
        "user.crypto.biometric",
        "Biometric availability bridge failed",
    )
    .is_some_and(|s| s.trim().eq_ignore_ascii_case("true"))
}

#[cfg(all(
    any(target_os = "ios", target_os = "macos", target_os = "android"),
    feature = "mobile"
))]
fn plugin_authenticate(plugin: &BiometricPlugin, reason: &str) -> bool {
    native_call(
        with_autorelease_pool(|| Ok::<_, jni::errors::Error>(plugin.authenticate(reason))),
        "user.crypto.biometric",
        "Biometric authentication bridge failed",
    )
    .is_some_and(|s| s.trim().eq_ignore_ascii_case("true"))
}

#[cfg(all(
    any(target_os = "ios", target_os = "macos", target_os = "android"),
    feature = "mobile"
))]
fn can_check_impl() -> bool {
    new_plugin().is_ok_and(|plugin| plugin_can_check(&plugin))
}

#[cfg(all(
    any(target_os = "ios", target_os = "macos", target_os = "android"),
    feature = "mobile"
))]
fn authenticate_sync(reason: &str) -> bool {
    let Ok(plugin) = new_plugin() else {
        report_nonfatal(
            "user.crypto.biometric",
            "Biometric plugin unavailable",
            "plugin init failed",
        );
        return false;
    };
    plugin_authenticate(&plugin, reason)
}

#[cfg(not(any(
    all(any(target_os = "ios", target_os = "macos"), feature = "mobile"),
    target_os = "android",
    target_os = "windows"
)))]
const fn can_check_impl() -> bool {
    false
}

#[cfg(not(any(
    all(any(target_os = "ios", target_os = "macos"), feature = "mobile"),
    target_os = "android",
    target_os = "windows"
)))]
const fn authenticate_sync(_: &str) -> bool {
    false
}

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "android",
    target_os = "windows"
))]
const CAN_CHECK_CACHE_TTL: Duration = Duration::from_secs(30);

#[cfg(any(
    target_os = "ios",
    target_os = "macos",
    target_os = "android",
    target_os = "windows"
))]
static CAN_CHECK_CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Synchronous availability check — safe to call in component render functions.
///
/// Caches native FFI / `WinRT` availability probes briefly so repeated component
/// renders do not block the UI thread, while expiring quickly enough to pick up
/// biometric enrollment changes made in OS settings while the app is open.
#[cfg(any(
    all(any(target_os = "ios", target_os = "macos"), feature = "mobile"),
    target_os = "android",
    target_os = "windows"
))]
pub fn can_check() -> bool {
    let now = Instant::now();
    if let Some((checked_at, value)) = *recover_poison(CAN_CHECK_CACHE.lock())
        && now.saturating_duration_since(checked_at) < CAN_CHECK_CACHE_TTL
    {
        return value;
    }
    let value = can_check_impl();
    *recover_poison(CAN_CHECK_CACHE.lock()) = Some((now, value));
    value
}

#[cfg(not(any(
    all(any(target_os = "ios", target_os = "macos"), feature = "mobile"),
    target_os = "android",
    target_os = "windows"
)))]
#[must_use]
pub const fn can_check() -> bool {
    can_check_impl()
}

/// Async biometric availability check — runs `can_check` on the blocking pool.
pub async fn can_check_async() -> bool {
    spawn_blocking(can_check).await.unwrap_or(false)
}

/// Async biometric authentication. Runs the blocking FFI call via the
/// project's `spawn_blocking` helper (background thread on native, inline on
/// WASM). Join/panic errors are logged, not silently discarded.
pub async fn authenticate_async(reason: &str) -> bool {
    let reason = reason.to_owned();
    native_call(
        spawn_blocking(move || authenticate_sync(&reason)).await,
        "user.crypto.biometric",
        "Biometric authentication failed",
    )
    .unwrap_or(false)
}
