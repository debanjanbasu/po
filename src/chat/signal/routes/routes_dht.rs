//! Encode/decode of route-entry blobs persisted in identity-record route subkeys.

use std::{cmp::Reverse, sync::Arc};

use anyhow::{Context, Result, anyhow};
use dioxus::logger::tracing::{debug, info};
use veilid_core::{KeyPair, RecordKey};

use crate::{
    compression::codec::decompress_plain_limited,
    config,
    helpers::{
        async_utils::now_epoch_millis,
        capnp::capnp_u32,
        framing::{
            push_u16_len_prefixed, push_u32_len_prefixed, read_u16_len_prefixed,
            read_u32_len_prefixed,
        },
    },
    platform::device_id::device_id,
    storage::background_errors::report_nonfatal,
    user::identity_layout::SUBKEY_ROUTES as SUBKEY_ROUTE,
    veilid::{
        core::parse_record_key,
        store::{self, StoreMode},
    },
};

#[derive(Clone)]
pub(super) struct RouteEntry {
    pub(super) device_id: Arc<str>,
    pub(super) timestamp: u64,
    pub(super) blob: Vec<u8>,
}

const MAX_DECOMPRESSED_ROUTE_BYTES: usize = 32 * 1024;

fn encode_route_entries(entries: &[RouteEntry]) -> Vec<u8> {
    let cap: usize = 4 + entries
        .iter()
        .map(|e| 2 + e.device_id.len() + 8 + 4 + e.blob.len())
        .sum::<usize>();
    let mut buf = Vec::with_capacity(cap);
    // The count is a bare u32 field, not a length-prefixed payload — matching
    // decode_route_entries' raw `data.get(..4)` read.
    buf.extend_from_slice(&capnp_u32(entries.len()).to_le_bytes());
    for entry in entries {
        push_u16_len_prefixed(&mut buf, entry.device_id.as_bytes());
        buf.extend_from_slice(&entry.timestamp.to_le_bytes());
        push_u32_len_prefixed(&mut buf, &entry.blob);
    }
    buf
}

fn decode_route_entries(data: &[u8], source: &str) -> Vec<RouteEntry> {
    let mut entries = Vec::new();
    // The 4-byte count is a bare u32 field, not a length-prefixed payload, so
    // it stays a raw read; every entry payload below goes through the shared
    // bounds-checked readers in helpers::framing.
    let Some(count_bytes) = data.get(..4) else {
        report_nonfatal(
            "chat.signal",
            "Route blob too short",
            format!("source={source}: blob {} bytes", data.len()),
        );
        return entries;
    };
    let count = u32::from_le_bytes([
        count_bytes[0],
        count_bytes[1],
        count_bytes[2],
        count_bytes[3],
    ]) as usize;
    if count == 0 || count > config::route::MAX_ENTRIES {
        report_nonfatal(
            "chat.signal",
            "Invalid route entry count",
            format!("source={source}: count={count} blob={} bytes", data.len()),
        );
        return entries;
    }
    let mut pos = 4;
    for _ in 0..count {
        // All framing goes through the shared bounds-checked readers; a
        // truncated entry breaks the loop exactly as before.
        let Ok((id_bytes, next)) = read_u16_len_prefixed(data, pos, "route device id") else {
            break;
        };
        let device_id = String::from_utf8_lossy(id_bytes);
        pos = next;
        let Some(ts_bytes) = data.get(pos..pos + 8) else {
            break;
        };
        let timestamp = u64::from_le_bytes(ts_bytes.try_into().unwrap_or_default());
        pos += 8;
        let Ok((blob, next)) = read_u32_len_prefixed(data, pos, "route blob") else {
            break;
        };
        entries.push(RouteEntry {
            device_id: Arc::from(device_id.as_ref()),
            timestamp,
            blob: blob.to_vec(),
        });
        pos = next;
    }
    if entries.len() < count {
        report_nonfatal(
            "chat.signal",
            "Route entries truncated",
            format!(
                "source={source}: declared {count} but only decoded {} (blob {} bytes)",
                entries.len(),
                data.len()
            ),
        );
    }
    entries
}

/// Read-modify-write: publish our device route blob in identity subkey 8.
pub(super) async fn publish_route_to_dht(
    dht_key: RecordKey,
    keypair: Option<KeyPair>,
    blob: &[u8],
) -> Result<()> {
    let now = now_epoch_millis();
    let dev_id = device_id().await?;

    store::with_record(&dht_key, keypair.as_ref(), |key| async move {
        let existing = match store::read_subkey_raw(&key, SUBKEY_ROUTE, true).await? {
            Some(data) => decode_route_entries(
                &decompress_plain_limited(data, MAX_DECOMPRESSED_ROUTE_BYTES).await?,
                "self-publish",
            ),
            None => Vec::new(),
        };

        let mut entries: Vec<RouteEntry> = existing
            .into_iter()
            .filter(|e| {
                e.device_id.as_ref() != dev_id.as_ref()
                    && now.saturating_sub(e.timestamp) <= config::route::FRESHNESS_MS
            })
            .collect();

        if entries.len() >= config::route::MAX_ENTRIES - 1 {
            entries.sort_by_key(|e| Reverse(e.timestamp));
            entries.truncate(config::route::MAX_ENTRIES - 1);
        }

        entries.push(RouteEntry {
            device_id: dev_id,
            timestamp: now,
            blob: blob.to_vec(),
        });

        // Superseded: another of this account's devices republished its route
        // concurrently — fail so the publish retry loop re-reads and re-merges.
        store::store_with_mode(
            &key,
            SUBKEY_ROUTE,
            encode_route_entries(&entries),
            None,
            StoreMode::Unencrypted,
        )
        .await?
        .require_written()
    })
    .await
}

/// Load all non-stale route entries for a peer from DHT subkey 8.
pub(super) async fn load_routes_from_dht(identity_key: &str) -> Result<Vec<RouteEntry>> {
    let key = parse_record_key(identity_key).context("invalid peer identity route key")?;
    let now = now_epoch_millis();

    let data = store::with_record(&key, None::<&KeyPair>, |k| async move {
        match store::read_subkey_raw(&k, SUBKEY_ROUTE, true).await? {
            Some(raw) => decompress_plain_limited(raw, MAX_DECOMPRESSED_ROUTE_BYTES)
                .await
                .map(Some),
            None => Ok(None),
        }
    })
    .await;

    let data = match data {
        Ok(Some(data)) => data,
        Ok(None) => {
            return Ok(Vec::new());
        }
        Err(e) => {
            debug!("Route subkey unreadable for {identity_key}: {e:#}");
            return Err(e).context("read peer route subkey");
        }
    };

    let decoded = decode_route_entries(&data, &format!("peer-load:{identity_key}"));
    if decoded.is_empty() {
        return Err(anyhow!("route subkey contains no decodable entries"));
    }
    let total = decoded.len();
    let newest_age_ms = decoded
        .iter()
        .map(|e| now.saturating_sub(e.timestamp))
        .min();
    let fresh: Vec<RouteEntry> = decoded
        .into_iter()
        .filter(|e| now.saturating_sub(e.timestamp) <= config::route::FRESHNESS_MS)
        .collect();
    if fresh.is_empty()
        && let Some(age_ms) = newest_age_ms
    {
        // Entries exist but none are fresh: the peer's devices stopped
        // re-stamping (suspended, offline, or running a build without
        // the route heartbeat). The age says how long ago the newest
        // device was alive.
        info!(
            "All {total} route entries for {identity_key} are stale (newest {}s old)",
            age_ms / 1000,
        );
    } else if fresh.len() < total {
        debug!(
            "Route entries for {identity_key}: {}/{total} within freshness window",
            fresh.len(),
        );
    }
    Ok(fresh)
}
