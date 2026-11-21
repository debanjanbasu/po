//! Pairing progress beacon — coarse cross-device sync milestones.
//!
//! While a pairing QR is on screen, the sharing device has no idea whether
//! the other device scanned it, is mid-sync, or finished. The new device
//! closes that gap by writing at most three coarse milestones to
//! [`identity_layout::SUBKEY_PAIRING_BEACON`] on the shared identity record
//! (it holds the account keypair from the pairing payload, so the write is
//! authorized). The sharing device learns the milestone from its identity
//! watch — with a read fallback inside that watch handler for a missed edge —
//! and renders it from a memo. Nothing polls the subkey while the QR is up.
//!
//! Correlation: both devices independently derive the same [`PairingId`] by
//! hashing the base64 pairing payload — unique per QR generation (the
//! payload embeds a random nonce), with no payload-format change. Stale
//! beacons from earlier pairings fail the id comparison and are ignored.
//!
//! The beacon is AEAD-encrypted with the password-derived DHT secret and
//! deliberately avoids dictionary compression: a fresh device may not have
//! the trained dictionary when it writes the first milestone.

use std::str;

use crate::helpers::signals::{GlobalSignal, Signal};
use anyhow::{Context, Result};
use veilid_core::{KeyPair, RecordKey, SharedSecret};

use crate::{
    config,
    helpers::{
        async_utils::{now_epoch_secs, sleep_ms},
        encryption::crypto_system_sync,
    },
    platform::device_id::device_label,
    session::{epoch::spawn_session_detached, phase::SESSION},
    storage::background_errors::report_nonfatal,
    user::{dht_secret_for_username, identity_layout},
    veilid::store::{self, StoreMode},
};

/// Correlates a beacon with one specific QR generation.
pub type PairingId = [u8; 8];

const BEACON_VERSION: u8 = 1;
/// `version(1) + pairing_id(8) + milestone(1) + timestamp(8) + label_len(1)`.
const BEACON_HEADER_LEN: usize = 1 + 8 + 1 + 8 + 1;

/// Coarse milestones the new device reports, in order. At most one write per
/// milestone per pairing run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeaconMilestone {
    /// Payload decrypted and identity fetched — the device is joining.
    Accepted,
    /// Admitted; hydrating app state, child records, and conversations.
    Syncing,
    /// Everything synced on the new device.
    Complete,
}

impl BeaconMilestone {
    const fn to_wire(self) -> u8 {
        match self {
            Self::Accepted => 1,
            Self::Syncing => 2,
            Self::Complete => 3,
        }
    }

    const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Accepted),
            2 => Some(Self::Syncing),
            3 => Some(Self::Complete),
            _ => None,
        }
    }
}

/// One decoded beacon value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingBeacon {
    pub pairing_id: PairingId,
    pub milestone: BeaconMilestone,
    pub at_epoch_secs: u64,
    /// Label of the device that wrote the beacon (may be empty).
    pub device_label: String,
}

impl PairingBeacon {
    /// True when this beacon belongs to the QR generated at `generated_at`
    /// with `pairing_id`. The timestamp slack tolerates clock skew between
    /// the two devices; stale beacons from earlier pairings fail the check.
    #[must_use]
    pub fn matches(&self, pairing_id: PairingId, generated_at: u64) -> bool {
        self.pairing_id == pairing_id
            && self
                .at_epoch_secs
                .saturating_add(config::pairing::BEACON_TIMESTAMP_SLACK_SECS)
                >= generated_at
    }
}

/// Derives the pairing id both devices share: a salted hash of the base64
/// pairing payload, truncated to 8 bytes.
pub fn pairing_id_for_payload(base64_payload: &str) -> Result<PairingId> {
    let crypto = crypto_system_sync().context("crypto system not available")?;
    let mut material = Vec::with_capacity(b"oo:pair:beacon:v1:".len() + base64_payload.len());
    material.extend_from_slice(b"oo:pair:beacon:v1:");
    material.extend_from_slice(base64_payload.as_bytes());
    let digest = crypto.generate_hash(&material).value().bytes();
    let mut id = PairingId::default();
    id.copy_from_slice(digest.get(..8).context("hash digest too short")?);
    Ok(id)
}

/// Truncates a device label to the wire budget on a char boundary.
fn bounded_label(label: &str) -> &str {
    label
        .get(..label.floor_char_boundary(config::pairing::BEACON_MAX_LABEL_BYTES))
        .unwrap_or_default()
}

fn encode_beacon(beacon: &PairingBeacon) -> Vec<u8> {
    let label = bounded_label(&beacon.device_label);
    let mut out = Vec::with_capacity(BEACON_HEADER_LEN + label.len());
    out.push(BEACON_VERSION);
    out.extend_from_slice(&beacon.pairing_id);
    out.push(beacon.milestone.to_wire());
    out.extend_from_slice(&beacon.at_epoch_secs.to_le_bytes());
    // `bounded_label` caps at BEACON_MAX_LABEL_BYTES (< u8::MAX); clamp defensively.
    out.push(u8::try_from(label.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(label.as_bytes());
    out
}

/// Decodes a beacon; `None` for unknown versions or malformed bytes (e.g.
/// junk written by a future app version).
fn decode_beacon(bytes: &[u8]) -> Option<PairingBeacon> {
    if bytes.len() < BEACON_HEADER_LEN || bytes[0] != BEACON_VERSION {
        return None;
    }
    let mut pairing_id = PairingId::default();
    pairing_id.copy_from_slice(&bytes[1..9]);
    let milestone = BeaconMilestone::from_wire(bytes[9])?;
    let at_epoch_secs = u64::from_le_bytes(bytes[10..18].try_into().ok()?);
    let label_len = bytes[18] as usize;
    let label_bytes = bytes.get(BEACON_HEADER_LEN..BEACON_HEADER_LEN + label_len)?;
    let device_label = str::from_utf8(label_bytes).ok()?.to_owned();
    Some(PairingBeacon {
        pairing_id,
        milestone,
        at_epoch_secs,
        device_label,
    })
}

/// Writes one milestone to the beacon subkey. Best-effort: callers wrap in
/// `nonfatal!` — a beacon failure must never fail the pairing itself.
///
/// Transient network errors (`TryAgain`/offline during dial-info churn) are
/// retried a few times with backoff: a milestone lost to a flapping network
/// would leave the share device blind to pairing progress.
pub async fn write_milestone(
    dht_key: &RecordKey,
    keypair: &KeyPair,
    secret: &SharedSecret,
    pairing_id: PairingId,
    milestone: BeaconMilestone,
) -> Result<()> {
    let device_label = device_label().await.unwrap_or_default();
    let beacon = PairingBeacon {
        pairing_id,
        milestone,
        at_epoch_secs: now_epoch_secs(),
        device_label,
    };
    let bytes = encode_beacon(&beacon);
    let mut waits = config::pairing::BEACON_WRITE_RETRY_WAITS_MS.into_iter();
    loop {
        match write_milestone_once(dht_key, keypair, secret, &bytes).await {
            Err(e) if store::is_transient_network_error(&e) => match waits.next() {
                Some(wait_ms) => sleep_ms(wait_ms).await,
                None => return Err(e),
            },
            result => return result,
        }
    }
}

async fn write_milestone_once(
    dht_key: &RecordKey,
    keypair: &KeyPair,
    secret: &SharedSecret,
    bytes: &[u8],
) -> Result<()> {
    let bytes = bytes.to_vec();
    let secret = secret.clone();
    store::with_record(dht_key, Some(keypair), |key| async move {
        // Superseded is benign: a newer milestone already landed, and newest
        // wins for a coarse progress beacon.
        let _outcome = store::store_with_mode(
            &key,
            identity_layout::SUBKEY_PAIRING_BEACON,
            bytes,
            Some(&secret),
            StoreMode::PlainEncrypted,
        )
        .await?;
        Ok(())
    })
    .await
}

/// The QR generation currently (or recently) displayed by share mode.
/// Registered when a QR is generated; cleared on share teardown. Global —
/// not component state — so beacon-driven progress stays derivable (and the
/// activity bar keeps showing it) after the pairing screen unmounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveShare {
    pairing_id: PairingId,
    generated_at: u64,
}

static ACTIVE_SHARE: std::sync::LazyLock<GlobalSignal<Option<ActiveShare>>> =
    std::sync::LazyLock::new(|| Signal::global(|| None));

/// A beacon plus when *this* device observed it. The beacon's own timestamp
/// comes from the other device's clock, so local display windows (e.g. how
/// long "synced" lingers) must not trust it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedBeacon {
    beacon: PairingBeacon,
    observed_at: u64,
}

/// Latest beacon from the identity-record DHT watch (push), or from the watch
/// handler's read fallback when an edge was missed. Consumed through
/// [`share_sync_status`], which the share UI reads as a memo.
static REMOTE_BEACON: std::sync::LazyLock<GlobalSignal<Option<ObservedBeacon>>> =
    std::sync::LazyLock::new(|| Signal::global(|| None));

/// Marks a freshly generated QR as the active share and drops any beacon
/// from an earlier generation.
pub fn register_active_share(pairing_id: PairingId, generated_at: u64) {
    let share = ActiveShare {
        pairing_id,
        generated_at,
    };
    *ACTIVE_SHARE.write() = Some(share);
    *REMOTE_BEACON.write() = None;
    let window_secs = config::e2e::pairing_ttl_secs()
        .saturating_add(config::pairing::SHARE_SYNC_HOLD_GRACE_SECS)
        .saturating_add(config::pairing::SHARE_COMPLETE_LINGER_SECS);
    spawn_session_detached(async move {
        sleep_ms(window_secs.saturating_mul(1_000)).await;
        if *ACTIVE_SHARE.peek() == Some(share) {
            clear_active_share();
        }
    });
}

/// Share teardown (expiry, completion, regeneration reset).
pub fn clear_active_share() {
    *ACTIVE_SHARE.write() = None;
    *REMOTE_BEACON.write() = None;
}

/// Publishes a freshly observed beacon (watch push or poll read).
pub fn publish_remote_beacon(beacon: PairingBeacon) {
    let is_complete = beacon.milestone == BeaconMilestone::Complete;
    let pairing_id = beacon.pairing_id;
    *REMOTE_BEACON.write() = Some(ObservedBeacon {
        beacon,
        observed_at: now_epoch_secs(),
    });
    if is_complete {
        spawn_session_detached(async move {
            sleep_ms(config::pairing::SHARE_COMPLETE_LINGER_SECS.saturating_mul(1_000)).await;
            if ACTIVE_SHARE
                .peek()
                .is_some_and(|share| share.pairing_id == pairing_id)
            {
                clear_active_share();
            }
        });
    }
}

/// Validated sync status of the device pairing against the active QR:
/// `None` when no share is active, the beacon belongs to another
/// generation, the share window (TTL + post-expiry sync grace) lapsed, or a
/// completed sync has already lingered long enough. Reads global signals,
/// so components and memos consuming it re-render on beacon updates.
#[must_use]
#[allow(clippy::significant_drop_tightening)] // the guards span field reads; cloning out would defeat them
pub fn share_sync_status() -> Option<(BeaconMilestone, String)> {
    let share = (*ACTIVE_SHARE.read())?;
    let session = &*SESSION;
    session.read().authenticated_user()?;
    let now = now_epoch_secs();
    let window = config::e2e::pairing_ttl_secs()
        .saturating_add(config::pairing::SHARE_SYNC_HOLD_GRACE_SECS)
        .saturating_add(config::pairing::SHARE_COMPLETE_LINGER_SECS);
    if now > share.generated_at.saturating_add(window) {
        return None;
    }
    let guard = REMOTE_BEACON.read();
    let observed = guard.as_ref()?;
    if !observed
        .beacon
        .matches(share.pairing_id, share.generated_at)
    {
        return None;
    }
    if observed.beacon.milestone == BeaconMilestone::Complete
        && now
            > observed
                .observed_at
                .saturating_add(config::pairing::SHARE_COMPLETE_LINGER_SECS)
    {
        return None;
    }
    Some((
        observed.beacon.milestone,
        observed.beacon.device_label.clone(),
    ))
}

/// Identity-watch entry point: decodes a changed beacon subkey and publishes
/// it to [`REMOTE_BEACON`]. Uses the value delivered with the `ValueChange`
/// when present, falling back to a network read. Best-effort — undecodable
/// bytes (foreign versions, stale garbage) are ignored.
pub async fn handle_watch_change(dht_key: RecordKey, raw: Option<Vec<u8>>) {
    let username = SESSION
        .peek()
        .authenticated_user()
        .map(|u| u.username().to_owned());
    let Some(username) = username else { return };
    let Some(secret) = dht_secret_for_username(&username).await else {
        return;
    };

    let beacon = match raw {
        Some(bytes) => store::decode_with_mode(&bytes, Some(&secret), StoreMode::PlainEncrypted)
            .await
            .ok()
            .and_then(|decoded| decode_beacon(&decoded)),
        None => match read_beacon(&dht_key, &secret).await {
            Ok(beacon) => beacon,
            Err(e) => {
                report_nonfatal(
                    "user.pairing_beacon",
                    "Failed to read pairing beacon after watch change",
                    e,
                );
                None
            }
        },
    };

    if let Some(beacon) = beacon {
        publish_remote_beacon(beacon);
    }
}

/// Reads the current beacon from the network (cache bypassed so the sharing
/// device sees the other device's write). `None` when the subkey was never
/// written or holds bytes this version cannot decode.
pub async fn read_beacon(
    dht_key: &RecordKey,
    secret: &SharedSecret,
) -> Result<Option<PairingBeacon>> {
    let secret = secret.clone();
    store::with_record(dht_key, None::<&KeyPair>, |key| async move {
        let Some(bytes) = store::load_opt(
            &key,
            identity_layout::SUBKEY_PAIRING_BEACON,
            &secret,
            true,
            StoreMode::PlainEncrypted,
        )
        .await?
        else {
            return Ok(None);
        };
        Ok(decode_beacon(&bytes))
    })
    .await
}
