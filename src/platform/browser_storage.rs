//! Whether this browser will let the app save anything, asked before Veilid
//! builds its config.
//!
//! The web build's entire local store hangs off one `localStorage` write: on
//! wasm Veilid picks `LocalStorage` for a Window context
//! (`veilid-core`, `protected_store/wasm.rs::resolve_backend`), and its table
//! store then puts the device encryption key there before it opens anything
//! (`table_store/mod.rs::startup`, `save_device_encryption_key`). A browser
//! that refuses that write — Private Browsing, cookies/site data blocked, a
//! full origin quota — leaves the app either looping on its startup splash or,
//! if the key is later evicted while the rows survive, wiping the tables it
//! cannot decrypt.
//!
//! The reason for the refusal exists in exactly one place, and Veilid's
//! mapping discards it: `map_js_error` formats `JsValue::as_string()`, which is
//! `None` for a `DOMException`, so the app is handed
//! `failed to write local storage: ` with an empty detail. This probe reads the
//! `DOMException` name where it is still present, then turns it into a sentence
//! the user can act on — the same reason the backup refusal names the account
//! mismatch rather than the password.
//!
//! Known gap: this asks about `localStorage` only. A browser that allows
//! `localStorage` but blocks `IndexedDB` (where the rows live) still fails
//! somewhere further in, with Veilid's generic
//! `failed to commit a transaction to IndexedDB`.

use thiserror::Error;
use wasm_bindgen::JsValue;
use web_sys::{Storage, window};

use crate::storage::background_errors::{ErrorSeverity, report_nonfatal_with_severity};

/// The app's own probe key. Namespaced to the probe, so a run that dies between
/// the write and the cleanup can only leave this one byte behind.
const PROBE_KEY: &str = "oo.storage.probe";

/// Why the browser will not persist this app's data.
///
/// `Display` is the user-facing text: each variant names what happened and what
/// to do about it, because a user cannot act on a cause they cannot see.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StorageProblem {
    /// No `window`, or no `localStorage` object behind it.
    #[error(
        "This browser is not giving the app access to site storage, so anything it saves will be \
         lost on reload. If you are in a private window, open the app in a normal one; otherwise \
         allow cookies and site data for this site, then reload."
    )]
    Unreachable,
    /// The origin's storage quota is exhausted (`QuotaExceededError`).
    #[error(
        "This browser's storage for this site is full, so the app cannot save anything. Free up \
         space on the device (or clear this site's data), then reload."
    )]
    Full,
    /// `setItem` threw something else — usually a `SecurityError` from blocked
    /// site data. `name` is the `DOMException` name, which is the detail
    /// Veilid's own error mapping throws away.
    #[error(
        "This browser refused to save the app's data (browser error `{name}`). Allow cookies and \
         site data for this site, then reload."
    )]
    Refused { name: String },
}

/// Probes site storage for a condition that makes the app's local store
/// impossible. `None` means the browser accepted a write and read it back.
///
/// Synchronous by construction: every web API it touches is.
#[must_use]
pub fn probe() -> Option<StorageProblem> {
    let Some(store) = local_storage() else {
        return Some(StorageProblem::Unreachable);
    };
    match store.set_item(PROBE_KEY, "1") {
        Ok(()) => {
            // Read back because a browser can accept the call and keep nothing
            // (ephemeral storage modes), which would otherwise look healthy here
            // and fail as a wipe later. `Ok(None)` is that case; a failed *read*
            // is not evidence against storage — the write was accepted — and
            // must not turn a hiccup into a permanent startup refusal.
            let verdict = match store.get_item(PROBE_KEY) {
                Ok(Some(value)) if value == "1" => None,
                Ok(_) => Some(StorageProblem::Refused {
                    name: "write discarded".to_owned(),
                }),
                Err(_) => None,
            };
            if let Err(error) = store.remove_item(PROBE_KEY) {
                // A `JsValue` has no `Display`, and this line is diagnostics
                // only — `Debug` is the whole of it.
                report_nonfatal_with_severity(
                    "platform.browser_storage",
                    ErrorSeverity::Silent,
                    "Storage probe cleanup failed",
                    format!("{error:?}"),
                );
            }
            verdict
        }
        Err(error) => Some(classify(&error)),
    }
}

fn local_storage() -> Option<Storage> {
    window()?.local_storage().ok().flatten()
}

/// Names a `DOMException`. The `name` property is read reflectively because a
/// `DOMException` is an object: `JsValue::as_string()` yields `None` for it,
/// which is exactly how the cause is lost upstream.
fn classify(error: &JsValue) -> StorageProblem {
    let name = js_sys::Reflect::get(error, &JsValue::from_str("name"))
        .ok()
        .and_then(|value| value.as_string())
        .unwrap_or_default();
    if name == "QuotaExceededError" {
        StorageProblem::Full
    } else if name.is_empty() {
        StorageProblem::Refused {
            name: "unidentified".to_owned(),
        }
    } else {
        StorageProblem::Refused { name }
    }
}
