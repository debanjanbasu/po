//! Native C and JNI entry points called into Rust by the iOS/macOS Swift
//! lifecycle observer (`DeepLinkPlugin.swift`) and Android `MainActivity.kt`.
//!
//! Isolated in this single module with `#![allow(unsafe_code)]` solely for the
//! Rust 2024 `#[unsafe(no_mangle)]` export attributes, keeping the rest of the
//! crate under `unsafe_code = "deny"` without separate bridge subcrates.

#![allow(unsafe_code)]

#[cfg(target_os = "android")]
use std::{mem, sync::Mutex};

#[cfg(target_os = "android")]
use dioxus::logger::tracing::warn;
#[cfg(target_os = "android")]
use jni::{
    EnvUnowned,
    errors::{Result as JniResult, ThrowRuntimeExAndDefault},
    jni_sig, jni_str,
    objects::{JObject, JObjectArray, JString},
    sys::jboolean,
};
use tokio::sync::Notify;

#[cfg(target_os = "android")]
use crate::helpers::store_lock::recover_poison;
#[cfg(target_os = "android")]
use crate::storage::background_errors::report_nonfatal;

static ACTIVITY_NOTIFY: Notify = Notify::const_new();
static BACKGROUND_NOTIFY: Notify = Notify::const_new();

#[cfg(target_os = "android")]
static PENDING_DEEP_LINK_URL: Mutex<String> = Mutex::new(String::new());

/// Harness seam values carried in the launch intent's extras.
///
/// Android has no environment channel for an app started by `am start`, so
/// the harness passes them as intent extras instead. `MainActivity.onCreate`
/// hands them to Rust through JNI; `config::e2e` reads them from here.
#[cfg(target_os = "android")]
static E2E_SEAMS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Whether `MainActivity` last reported that `ACCESS_LOCAL_NETWORK` is granted.
///
/// `None` until the platform has answered, and on targets with no such
/// permission, so consumers can stay quiet instead of claiming a denial. The
/// answer matters because a denial is otherwise silent: Veilid keeps attempting
/// peer discovery on the LAN against a permission the app does not hold.
#[cfg(target_os = "android")]
static LOCAL_NETWORK_GRANTED: Mutex<Option<bool>> = Mutex::new(None);

/// Reads the local-network permission state reported by `MainActivity`.
///
/// `None` means "not answered yet", which is also every non-Android target: the
/// equivalent prompt there is TCC, which exposes no read API.
#[cfg(target_os = "android")]
#[must_use]
pub fn local_network_granted() -> Option<bool> {
    *recover_poison(LOCAL_NETWORK_GRANTED.lock())
}

/// Reads the local-network permission state reported by `MainActivity`.
///
/// Always `None` where there is no such permission to answer.
#[cfg(not(target_os = "android"))]
#[must_use]
pub const fn local_network_granted() -> Option<bool> {
    None
}

/// Records the local-network permission state `MainActivity` reports.
#[cfg(target_os = "android")]
fn set_local_network_granted(granted: bool) {
    let previous = {
        let mut state = recover_poison(LOCAL_NETWORK_GRANTED.lock());
        let previous = *state;
        *state = Some(granted);
        previous
    };
    // `MainActivity.onResume` is level-triggered, so report only the transition
    // into denial — the user-visible error list would otherwise collect a fresh
    // entry on every resume while the permission stays off.
    if !granted && previous != Some(false) {
        report_nonfatal(
            "platform.native_bridge",
            "Local network access denied",
            "Pairing with peers on the local network will fail until po is allowed \
             to access the local network in Settings",
        );
    }
}

pub async fn wait_activity_change() {
    ACTIVITY_NOTIFY.notified().await;
}

pub async fn wait_background() {
    BACKGROUND_NOTIFY.notified().await;
}

#[cfg(target_os = "android")]
pub fn take_pending_deep_link_url() -> String {
    mem::take(&mut *recover_poison(PENDING_DEEP_LINK_URL.lock()))
}

/// Reads a harness seam value carried in the launch intent's extras.
#[cfg(target_os = "android")]
#[must_use]
pub fn e2e_seam(key: &str) -> Option<String> {
    recover_poison(E2E_SEAMS.lock())
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty())
}

/// Stores the harness seam values carried in the launch intent's extras.
#[cfg(target_os = "android")]
pub fn set_e2e_seams(values: Vec<(String, String)>) {
    *recover_poison(E2E_SEAMS.lock()) = values;
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
#[unsafe(no_mangle)]
pub extern "C" fn po_notify_app_activity_changed() {
    ACTIVITY_NOTIFY.notify_one();
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
#[unsafe(no_mangle)]
pub extern "C" fn po_notify_app_background() {
    BACKGROUND_NOTIFY.notify_one();
}

/// Keeps the two notify exports above in the binary.
///
/// Swift resolves them by name through `dlsym`, so nothing in Rust calls them
/// and the linker is free to drop them — which it does under `lto = "fat"`,
/// leaving `dlsym` to return null and every native lifecycle notification a
/// silent no-op. `#[used]` pins the function pointers; the `#[unsafe(no_mangle)]`
/// attributes put them in the dynamic export table `dlsym` reads.
#[cfg(any(target_os = "ios", target_os = "macos"))]
#[used]
static KEEP_NOTIFY_EXPORTS: [unsafe extern "C" fn(); 2] =
    [po_notify_app_activity_changed, po_notify_app_background];

// The JNI entry points below spell their exported symbol with `export_name`
// instead of taking a `non_snake_case` allow: the JVM looks the symbol up
// verbatim, and writing it out keeps the Rust idents snake_case.
/// The context seed `veilid_core_setup_android` needs before `api_startup()`.
///
/// The local-network permission is checked and requested in `MainActivity` —
/// through the `ActivityResultLauncher` its `WryActivity` superclass registers —
/// because the answer has to come back through that callback; the raw
/// `Activity.requestPermissions` this used to call hands the result to
/// `onRequestPermissionsResult`, which nothing in this app observes.
#[cfg(target_os = "android")]
#[unsafe(export_name = "Java_dev_dioxus_main_MainActivity_initVeilidAndroid")]
pub extern "system" fn init_veilid_android(env: EnvUnowned, _activity: JObject, ctx: JObject) {
    veilid_core::veilid_core_setup_android(env, ctx);
}

/// Receives the local-network permission answer from `MainActivity`.
#[cfg(target_os = "android")]
#[unsafe(export_name = "Java_dev_dioxus_main_MainActivity_reportLocalNetworkAccess")]
pub extern "system" fn report_local_network_access(
    _env: EnvUnowned,
    _activity: JObject,
    granted: jboolean,
) {
    set_local_network_granted(granted);
}

#[cfg(target_os = "android")]
#[unsafe(export_name = "Java_dev_dioxus_main_MainActivity_notifyAppActivityChanged")]
pub extern "system" fn notify_app_activity_changed(_env: EnvUnowned, _activity: JObject) {
    ACTIVITY_NOTIFY.notify_one();
}

#[cfg(target_os = "android")]
#[unsafe(export_name = "Java_dev_dioxus_main_MainActivity_notifyAppBackgrounded")]
pub extern "system" fn notify_app_backgrounded(_env: EnvUnowned, _activity: JObject) {
    BACKGROUND_NOTIFY.notify_one();
}

#[cfg(target_os = "android")]
#[unsafe(export_name = "Java_dev_dioxus_main_MainActivity_cachePendingDeepLink")]
pub extern "system" fn cache_pending_deep_link(
    mut env: EnvUnowned,
    _activity: JObject,
    intent: JObject,
) {
    env.with_env(|env| -> JniResult<()> {
        if intent.is_null() {
            return Ok(());
        }
        let data = env
            .call_method(
                &intent,
                jni_str!("getDataString"),
                jni_sig!("()Ljava/lang/String;"),
                &[],
            )?
            .l()?;
        if !data.is_null() {
            let url = env.as_cast::<JString>(&data)?.try_to_string(env)?;
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                let mut pending = recover_poison(PENDING_DEEP_LINK_URL.lock());
                if !pending.is_empty() {
                    warn!("pending deep link was replaced before it was consumed");
                }
                trimmed.clone_into(&mut pending);
                drop(pending);
            }
        }
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}

/// Receives the harness seam values from the launch intent's extras.
///
/// Android cannot pass environment variables to an app started by `am start`,
/// so the harness carries them as intent extras named `po_e2e_<KEY>` (e.g.
/// `po_e2e_PO_E2E=1`). The keys are handed over as a `String[]` and the
/// values in the same order, which keeps the JNI signature simple and avoids
/// a map round-trip through `Bundle`.
#[cfg(target_os = "android")]
#[unsafe(export_name = "Java_dev_dioxus_main_MainActivity_cacheE2eSeams")]
pub extern "system" fn cache_e2e_seams(
    mut env: EnvUnowned,
    _activity: JObject,
    keys: JObjectArray<JString>,
    values: JObjectArray<JString>,
) {
    env.with_env(|env| -> JniResult<()> {
        let count = keys.len(env)?.min(values.len(env)?);
        let mut seams = Vec::with_capacity(count);
        for index in 0..count {
            let key = keys.get_element(env, index)?.try_to_string(env)?;
            let value = values.get_element(env, index)?.try_to_string(env)?;
            if !key.is_empty() && !value.is_empty() {
                seams.push((key, value));
            }
        }
        set_e2e_seams(seams);
        Ok(())
    })
    .resolve::<ThrowRuntimeExAndDefault>();
}
