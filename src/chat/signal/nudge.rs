//! Same-account device sync nudge — the fast lane for cross-device sync.
//!
//! After the sync queue publishes identity-scoped data to the DHT, the
//! writing device pokes the user's *other* devices over their private
//! routes so they pull immediately instead of waiting for a DHT watch
//! (`ValueChanged` is one-shot and unacknowledged, so it can be lost) or
//! the periodic inspect poll. Fire-and-forget: delivery failure is fine
//! because the durable paths (watches + polling) still converge.

use std::sync::Arc;

use tracing::debug;
use veilid_core::{KeyPair, RecordKey};

use crate::{platform::device_id::device_id, session::epoch::spawn_session_scoped};

use super::{
    routes::{prepare_signal_bytes_as, send_prepared_signal_excluding},
    wire::{PeerSignal, SignalPayload},
};

/// Signs and fans a [`PeerSignal::SyncNudge`] out to every sibling device
/// of `identity_key` (excluding this device). `scope` carries
/// [`crate::config::nudge`] bits naming what was published.
pub fn nudge_sibling_devices(identity_key: RecordKey, keypair: KeyPair, scope: u8) {
    if scope == 0 {
        return;
    }
    spawn_session_scoped(async move {
        let Ok(own_device) = device_id().await else {
            return;
        };
        let signal = PeerSignal::unsigned(SignalPayload::SyncNudge {
            identity_key: identity_key.clone(),
            scope,
            device_id: Arc::clone(&own_device),
        });
        let Some(message) = prepare_signal_bytes_as(&signal, &identity_key, &keypair).await else {
            return;
        };
        let identity_str = identity_key.to_string();
        let delivered = send_prepared_signal_excluding(
            &identity_str,
            message,
            Some(own_device.as_ref()),
            signal.is_durable(),
        )
        .await;
        debug!("Sync nudge sent: scope={scope:#04x} delivered={delivered}");
    });
}
