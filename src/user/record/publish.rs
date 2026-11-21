//! Post-publish verification for pairing: inspects the identity record,
//! re-pushes subkeys the network is missing, and reports remaining gaps.

use anyhow::Result;
use futures_util::future::join_all;
use veilid_core::{DHTRecordReport, DHTReportScope, KeyPair, RecordKey, ValueSeqNum};

use crate::{
    config::pairing::{PUBLISH_ENSURE_TIMEOUT, PUBLISH_VERIFY_BACKOFF_MS, PUBLISH_VERIFY_ROUNDS},
    helpers::async_utils::sleep_ms,
    nonfatal,
    storage::store_trait::SyncSurface,
    user::{
        identity_layout,
        sync_models::{SyncProfile, SyncSecurity},
    },
    veilid::{dict_sync, store},
};

/// Human-readable label for an identity-record subkey, for pairing diagnostics.
const fn identity_subkey_label(subkey: u32) -> &'static str {
    match subkey {
        SyncProfile::SUBKEY => "profile",
        SyncSecurity::SUBKEY => "security",
        identity_layout::SUBKEY_APP_STATE => "app state",
        identity_layout::SUBKEY_KEYPAIR => "keypair",
        identity_layout::SUBKEY_POINTERS => "pointers",
        dict_sync::SUBKEY_DICT_META => "dictionary metadata",
        dict_sync::SUBKEY_DICT_BLOB => "dictionary blob",
        _ => "data",
    }
}

/// Subkeys that exist locally but are missing (or stale) on the network.
fn missing_network_subkeys(
    entries: impl Iterator<Item = (u32, ValueSeqNum, ValueSeqNum)>,
) -> Vec<u32> {
    entries
        .filter_map(|(subkey, local, network)| {
            let local = local.to_option()?;
            match network.to_option() {
                Some(n) if n >= local => None,
                _ => Some(subkey),
            }
        })
        .collect()
}

fn missing_from_report(report: &DHTRecordReport) -> Vec<u32> {
    missing_network_subkeys(
        report
            .subkeys()
            .iter()
            .zip(report.local_seqs().iter().copied())
            .zip(report.network_seqs().iter().copied())
            .map(|((subkey, local), network)| (subkey, local, network)),
    )
}

/// Inspects the record, waiting out transient network failures (offline
/// window, flapping connection) with the verify backoff. The publish check
/// runs in the background while the pairing QR is already on screen, so
/// riding out a flap beats aborting the whole verification on the first
/// `TryAgain`.
async fn inspect_retry_transient(key: &RecordKey) -> Result<DHTRecordReport> {
    let mut round = 0;
    loop {
        match store::inspect_record(key, None, DHTReportScope::SyncGet).await {
            Err(e) if store::is_transient_network_error(&e) && round < PUBLISH_VERIFY_ROUNDS => {
                round += 1;
                sleep_ms(PUBLISH_VERIFY_BACKOFF_MS * round).await;
            }
            other => return other,
        }
    }
}

/// Ensures every locally written identity subkey is readable on the DHT.
///
/// Veilid skips open-time rehydration for records created in the current app
/// session, so subkeys written while the node had poor connectivity can stay
/// invisible to other devices until the app restarts — pairing then fails on
/// the other device with per-subkey read errors. This inspects the record,
/// re-pushes any missing subkey (re-writing identical bytes keeps the
/// sequence number but still runs the `SetValue` network fanout), and verifies
/// the network can serve them.
///
/// Returns labels of subkeys still unreadable on the network; empty means the
/// record is fully published.
pub async fn ensure_identity_published(
    dht_key: &RecordKey,
    keypair: &KeyPair,
) -> Result<Vec<&'static str>> {
    let missing = store::with_record_timeout(
        dht_key,
        Some(keypair),
        PUBLISH_ENSURE_TIMEOUT,
        |key| async move {
            // Push any queued offline writes first (no-op when none pending).
            nonfatal!(
                "user.record",
                "Failed to flush identity record before publish check",
                store::flush_dht_record(&key).await
            );

            let report = inspect_retry_transient(&key).await?;
            let mut missing = missing_from_report(&report);
            if missing.is_empty() {
                return Ok(missing);
            }

            // Re-push local values for the missing subkeys.
            let results = join_all(missing.iter().map(|&subkey| {
                let key = key.clone();
                async move {
                    match store::read_subkey_raw(&key, subkey, false).await? {
                        Some(bytes) => store::write_subkey_raw(&key, subkey, bytes).await,
                        None => Ok(()),
                    }
                }
            }))
            .await;
            for result in results {
                nonfatal!(
                    "user.record",
                    "Failed to re-publish identity subkey",
                    result
                );
            }

            for round in 1..=PUBLISH_VERIFY_ROUNDS {
                let report = inspect_retry_transient(&key).await?;
                missing = missing_from_report(&report);
                if missing.is_empty() || round == PUBLISH_VERIFY_ROUNDS {
                    break;
                }
                sleep_ms(PUBLISH_VERIFY_BACKOFF_MS * round).await;
            }
            Ok(missing)
        },
    )
    .await?;

    Ok(missing.into_iter().map(identity_subkey_label).collect())
}
