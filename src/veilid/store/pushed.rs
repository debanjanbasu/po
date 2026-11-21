//! The changed subkey's value, as `ValueChange` delivered it.
//!
//! Core commits a changed value locally *before* reporting it
//! (`storage_manager::watch_value::accept_single_value_change_and_report`) and
//! hands the bytes along in [`VeilidValueChange::value`]. Dropping them and
//! re-reading the subkey costs a full DHT GET — a network fanout for bytes the
//! event already carried — on the app's hottest event: a message arriving.
//!
//! This module carries that value from the change dispatcher to the watcher's
//! load path, so the re-read of *that one subkey* becomes a
//! [`decode_with_mode`] instead. Veilid attaches the value to the **first**
//! subkey of the change's range only, so [`PushedValue`] is keyed by subkey and
//! every load matches on it; any other changed subkey keeps its network read.

use anyhow::Result;
use bytes::Bytes;
use veilid_core::{RecordKey, SharedSecret, ValueData, VeilidValueChange};

use super::{StoreMode, decode_with_mode, mode::load_opt_with_mode};

/// A changed subkey's value, in stored (compressed + AEAD) form.
#[derive(Clone, Debug)]
pub struct PushedValue {
    /// The subkey these bytes belong to — the first of the change's range.
    subkey: u32,
    /// The value core delivered with the change.
    value: ValueData,
}

/// Pulls the delivered value out of a change, if it carries one.
///
/// `None` for the two value-less shapes, which keep their network read:
/// a coalesced merge (`change_dispatcher::merge_pending_change` unions several
/// subkeys' ranges, so no single subkey's bytes apply any more) and core's hint
/// path (`report_value_change_hint`), which reports a changed range without a
/// value.
#[must_use]
pub fn pushed_value(change: &VeilidValueChange) -> Option<PushedValue> {
    Some(PushedValue {
        subkey: change.subkeys.first()?,
        value: change.value.clone()?,
    })
}

/// Reads one subkey, serving it from the change's delivered value when that
/// value is this subkey's, and reading it back from the network otherwise.
///
/// The fallback is always a forced refresh: a caller is re-reading because the
/// subkey changed, so the persisted local copy — which may be this device's own
/// older write — must not be allowed to answer it.
pub async fn load_opt_preferring_push(
    key: &RecordKey,
    subkey: u32,
    secret: Option<&SharedSecret>,
    mode: StoreMode,
    pushed: Option<&PushedValue>,
) -> Result<Option<Bytes>> {
    if let Some(value) = pushed.filter(|p| p.subkey == subkey).map(|p| &p.value) {
        return decode_with_mode(value.data(), secret, mode).await.map(Some);
    }
    load_opt_with_mode(key, subkey, secret, true, mode).await
}
