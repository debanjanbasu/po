//! Knobs the tandem device-test harness uses ([docs/device-testing.md]).
//!
//! Honoured by debug builds and by any build that enables the off-by-default
//! `e2e-seams` feature (a release-profile harness build); every other build
//! compiles the accessors to a no-op, so a shipped binary can neither relocate
//! its data nor accept pairing credentials from the environment.

/// Reads a seam value from whichever channel this platform has.
///
/// The environment is the channel on desktop and iOS. Android cannot set
/// environment variables for an app started by `am start`, so the harness
/// carries the same values in the launch intent's extras
/// (`po_e2e_<KEY>`), which `MainActivity` hands to Rust over JNI.
///
/// Seam builds only.
#[cfg(any(debug_assertions, feature = "e2e-seams"))]
fn seam_value(key: &str) -> Option<String> {
    if let Ok(value) = std::env::var(key) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    #[cfg(target_os = "android")]
    if let Some(value) = crate::platform::native_bridge::e2e_seam(key) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    None
}

/// `PO_E2E=1` — this process is under the harness: emit the marker lines
/// the harness tails.
///
/// Native reads the environment. The browser has no environment channel —
/// `std::env::var` is a stub there that always reports "not present" — so
/// the harness sets `localStorage.po_e2e = "1"` instead. Storage, not a
/// query parameter: a query parameter does not survive the app's in-app
/// navigation, and `enabled()` is read at event time, not only at boot.
#[cfg(all(
    any(debug_assertions, feature = "e2e-seams"),
    not(target_arch = "wasm32")
))]
#[must_use]
pub fn enabled() -> bool {
    seam_value("PO_E2E").is_some_and(|value| value == "1")
}

#[cfg(all(any(debug_assertions, feature = "e2e-seams"), target_arch = "wasm32"))]
#[must_use]
pub fn enabled() -> bool {
    web_sys::window()
        .and_then(|window| window.local_storage().ok().flatten())
        .and_then(|store| store.get_item("po_e2e").ok().flatten())
        .is_some_and(|value| value == "1")
}

#[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
#[must_use]
pub const fn enabled() -> bool {
    false
}

/// `PO_E2E_PAIRING_TTL_SECS=<n>` — how long a pairing payload stays valid.
///
/// Only the side that *creates* a payload reads this: the payload carries its
/// own `expires_at`, so the expiry test is about the creator's value. The
/// harness shortens it so a scenario can wait out a real expiry in seconds
/// rather than the production five minutes; outside a seam build the value is
/// the constant, so a distributed build cannot be handed a shorter window.
#[cfg(all(
    any(debug_assertions, feature = "e2e-seams"),
    not(target_arch = "wasm32")
))]
#[must_use]
pub fn pairing_ttl_secs() -> u64 {
    seam_value("PO_E2E_PAIRING_TTL_SECS")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(super::pairing::TTL_SECS)
}

#[cfg(all(any(debug_assertions, feature = "e2e-seams"), target_arch = "wasm32"))]
#[must_use]
pub fn pairing_ttl_secs() -> u64 {
    web_sys::window()
        .and_then(|window| window.local_storage().ok().flatten())
        .and_then(|store| store.get_item("po_e2e_pairing_ttl_secs").ok().flatten())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(super::pairing::TTL_SECS)
}

#[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
#[must_use]
pub const fn pairing_ttl_secs() -> u64 {
    super::pairing::TTL_SECS
}

/// `PO_INSTANCE=<name>` — isolate this process's Veilid data dir and
/// namespace so two instances can run on one machine (Mac↔Mac tandem).
#[cfg(any(debug_assertions, feature = "e2e-seams"))]
#[must_use]
pub fn instance() -> Option<String> {
    seam_value("PO_INSTANCE")
}

#[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
#[must_use]
pub const fn instance() -> Option<String> {
    None
}

/// `PO_E2E_PAIR_PIN` + `PO_E2E_PAIR_PASSWORD` — what an unattended joiner
/// fills into the ordinary pairing form (validation and admission are
/// unchanged; the form's own auto-submit does the rest). Never logged.
#[cfg(any(debug_assertions, feature = "e2e-seams"))]
#[must_use]
pub fn pairing_credentials() -> Option<(String, String)> {
    let pin = seam_value("PO_E2E_PAIR_PIN")?;
    let password = seam_value("PO_E2E_PAIR_PASSWORD")?;
    Some((pin, password))
}

#[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
#[must_use]
pub const fn pairing_credentials() -> Option<(String, String)> {
    None
}

/// `PO_E2E_PAIR_PAYLOAD` — the pairing payload (deep link or raw
/// base64url) for an unattended joiner. Together with the PIN and
/// password it fills the whole form, so a device without a typing
/// channel can still pair. Seam builds only (`e2e-seams` or debug).
#[cfg(any(debug_assertions, feature = "e2e-seams"))]
#[must_use]
pub fn pairing_payload() -> Option<String> {
    seam_value("PO_E2E_PAIR_PAYLOAD")
}

#[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
#[must_use]
pub const fn pairing_payload() -> Option<String> {
    None
}

/// `PO_E2E_ACCOUNT_PIN` — the account's second factor, so an unattended
/// device can pass the PIN gate that follows admission (login, pairing).
/// Seam builds only (`e2e-seams` or debug); every other build compiles the
/// accessor to `None`.
#[cfg(any(debug_assertions, feature = "e2e-seams"))]
#[must_use]
pub fn account_pin() -> Option<String> {
    seam_value("PO_E2E_ACCOUNT_PIN")
}

#[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
#[must_use]
pub const fn account_pin() -> Option<String> {
    None
}

/// Emit one harness marker line, but only in a seam build under the
/// harness.
///
/// Collapses the `if e2e::enabled() { info!(target: "e2e", event = …) }`
/// guard that every marker site otherwise repeats. In a build without the
/// seams the whole call compiles away — the `enabled()` check is a
/// compile-time `false` — so a shipped binary emits nothing and pays
/// nothing.
///
/// The fields are forwarded verbatim to `tracing::info!`, so they use that
/// macro's own syntax (`name = %value`).
///
/// ```ignore
/// e2e::event!("conv_setup_enter");
/// e2e::event!("profile_merged", field = %field);
/// ```
macro_rules! event {
    ($event:expr $(,)?) => {
        if $crate::config::e2e::enabled() {
            dioxus::logger::tracing::info!(target: "e2e", event = $event);
        }
    };
    ($event:expr, $($field:tt)+) => {
        if $crate::config::e2e::enabled() {
            dioxus::logger::tracing::info!(target: "e2e", event = $event, $($field)+);
        }
    };
}

pub(crate) use event;
