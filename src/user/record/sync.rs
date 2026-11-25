//! Identity-record pulls from the DHT: bounded retry rounds, freshness
//! inspects, and the steady-state / pairing policy variants.

use anyhow::{Error as AnyhowError, Result, anyhow, bail};
use bytes::Bytes;
use dioxus::logger::tracing::{debug, warn};
use thiserror::Error;
use veilid_core::{DHTReportScope, KeyPair, RecordKey, SharedSecret, ValueSubkeyRangeSet};

use std::{io, time::Duration};

use crate::{
    config::{
        dht::{IDENTITY_SYNC_TIMEOUT as DHT_IDENTITY_SYNC_TIMEOUT, WATCHER_ROUND_WAITS_MS},
        pairing::{
            IDENTITY_SYNC_TIMEOUT as PAIRING_IDENTITY_SYNC_TIMEOUT, STALE_SOURCE_REPULL_WAITS_MS,
            SYNC_ROUND_WAITS_MS,
        },
    },
    helpers::{
        async_utils::{sleep_ms, spawn_blocking},
        concurrency,
    },
    models::app_state::AppState,
    storage::{
        automerge::{AutomergeSyncError, from_automerge},
        store_trait::SyncSurface,
    },
    user::{
        identity_layout,
        model::onboarding_complete,
        sync_models::{SyncPointers, SyncProfile, SyncSecurity},
    },
    veilid::store,
};

/// Marker context on [`sync_from_dht`] errors when every failed subkey read
/// was a network reachability failure (as opposed to decrypt/decompress).
/// Callers surface this as "record unreachable — check connectivity".
#[derive(Debug, Error)]
#[error("identity record unreachable on the DHT")]
pub struct RecordUnreachable;

/// Result of a full identity-record pull: hydrated values plus the raw
/// automerge doc bytes for the subkeys that carry local CRDT history
/// (profile/security), so callers can merge instead of overwrite.
pub struct IdentityPull {
    pub profile: SyncProfile,
    pub security: SyncSecurity,
    pub app_state: AppState,
    pub pointers: SyncPointers,
    pub profile_doc: Bytes,
    pub security_doc: Bytes,
    pub app_state_doc: Bytes,
}

impl IdentityPull {
    /// True when the pulled profile/security carry completed onboarding
    /// (mirrors [`crate::user::User::needs_onboarding`]). `false` on a
    /// pairing pull means the DHT copy predates the sharing device's
    /// post-onboarding publish — the share flow refuses to mint QR codes for
    /// non-onboarded identities, so the source account itself is always
    /// complete.
    #[must_use]
    pub const fn is_onboarded(&self) -> bool {
        onboarding_complete(
            self.profile.terms_accepted,
            self.profile.dob,
            !self.security.pin_hash.is_empty(),
        )
    }
}

/// Read-retry policy for identity subkey loads.
struct SyncPolicy {
    /// Wait before each round (ms); the array length is the round count.
    round_waits_ms: &'static [u64],
    /// Fan out one `SyncGet` inspect before each round so Veilid records the
    /// nodes holding this record in its per-record value-node cache. The
    /// subsequent value reads then start at the record's neighborhood
    /// instead of cold-walking a just-attached device's immature routing
    /// table — the pairing case, where every fanout exhausted its budget
    /// before ever finding the holders.
    seed_value_nodes: bool,
}

/// Pulls identity from DHT and returns merged sync data including pointers.
///
/// Steady-state variant (watcher): quick retries on a warm routing table.
pub async fn sync_from_dht(
    dht_key: RecordKey,
    password_secret: &SharedSecret,
) -> Result<IdentityPull> {
    sync_with_policy(
        &dht_key,
        password_secret,
        SyncPolicy {
            round_waits_ms: &WATCHER_ROUND_WAITS_MS,
            seed_value_nodes: false,
        },
        DHT_IDENTITY_SYNC_TIMEOUT,
    )
    .await
}

/// Pairing variant of [`sync_from_dht`]: a just-attached device keeps
/// failing DHT fanouts until its routing table matures, so this retries for
/// minutes with escalating waits and seeds the value-node cache with an
/// inspect before each round. A budget timeout is classified as
/// [`RecordUnreachable`] — from the user's view the record was never
/// reached, and the caller's message must say so rather than "internal
/// error".
pub async fn sync_from_dht_for_pairing(
    dht_key: &RecordKey,
    password_secret: &SharedSecret,
) -> Result<IdentityPull> {
    match sync_with_policy(
        dht_key,
        password_secret,
        SyncPolicy {
            round_waits_ms: &SYNC_ROUND_WAITS_MS,
            seed_value_nodes: true,
        },
        PAIRING_IDENTITY_SYNC_TIMEOUT,
    )
    .await
    {
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|io| io.kind() == io::ErrorKind::TimedOut) =>
        {
            Err(e.context(RecordUnreachable))
        }
        other => other,
    }
}

/// All four subkeys are written at account creation, so every one is
/// required: a read that comes back empty means no replica was found on the
/// network and is retried, then surfaced as [`RecordUnreachable`] — never
/// silently defaulted (a paired device must not start from a blank profile).
async fn sync_with_policy(
    dht_key: &RecordKey,
    password_secret: &SharedSecret,
    policy: SyncPolicy,
    budget: Duration,
) -> Result<IdentityPull> {
    store::with_record_timeout(dht_key, None::<&KeyPair>, budget, |key| async move {
        let subkeys: [(u32, &str); 4] = [
            (SyncProfile::SUBKEY, "profile"),
            (SyncSecurity::SUBKEY, "security"),
            (identity_layout::SUBKEY_APP_STATE, "app_state"),
            (identity_layout::SUBKEY_POINTERS, "pointers"),
        ];
        let mut loaded: [Option<Bytes>; 4] = [const { None }; 4];
        let mut errors: Vec<(usize, AnyhowError)> = Vec::new();
        let mut pending = Vec::with_capacity(subkeys.len());

        // Freshness verdict, once per sync: which subkeys the network holds
        // newer than this device's persisted local record store. Without
        // this, a non-forced read on a device that has run before returns
        // its own stale local copy and the whole pull merges to a no-op —
        // remote nav/theme/profile changes never land. Native watching
        // covers steady state; this covers login/resync catch-up.
        let newer = inspect_newer_subkeys(&key, &subkeys).await;

        for (round, wait_ms) in policy.round_waits_ms.iter().enumerate() {
            if *wait_ms > 0 {
                sleep_ms(*wait_ms).await;
            }

            pending.clear();
            pending.extend((0..subkeys.len()).filter(|&i| loaded[i].is_none()));

            // Best-effort cache seed: a failed inspect must never block the
            // round's reads — get_dht_value has its own fanout and can
            // succeed where inspect's consensus requirements fail.
            if policy.seed_value_nodes
                && round > 0
                && let Err(e) = seed_identity_value_nodes(&key, &subkeys).await
            {
                debug!(round, "identity sync: value-node seeding failed: {e:#}");
            }

            let results =
                load_identity_round(&key, &subkeys, &pending, password_secret, newer.as_ref())
                    .await;
            classify_round_results(round, &subkeys, results, &mut loaded, &mut errors);
            if errors.is_empty() {
                break;
            }
        }

        if !errors.is_empty() {
            // A missing subkey is a reachable-but-incomplete record, not a
            // network failure — retries cannot produce it, and the caller's
            // "check your connection" message would mislead.
            let all_network = errors
                .iter()
                .all(|(_, e)| e.downcast_ref::<store::DhtReadError>().is_some())
                && !errors
                    .iter()
                    .any(|(_, e)| e.downcast_ref::<store::DhtSubkeyMissing>().is_some());
            let detail = errors
                .iter()
                .map(|(i, e)| format!("subkey {} ({}): {e:#}", subkeys[*i].0, subkeys[*i].1))
                .collect::<Vec<_>>()
                .join("; ");
            let err = anyhow!("identity sync load failed: {detail}");
            return Err(if all_network {
                err.context(RecordUnreachable)
            } else {
                err
            });
        }

        let [
            Some(profile_bytes),
            Some(security_bytes),
            Some(app_state_bytes),
            Some(pointers_bytes),
        ] = loaded
        else {
            bail!("identity sync invariant violated: subkey loads incomplete");
        };
        let profile_doc = profile_bytes.clone();
        let security_doc = security_bytes.clone();
        let app_state_doc = app_state_bytes.clone();
        let (profile, security, app_state, pointers) = spawn_blocking(
            move || -> Result<
                (SyncProfile, SyncSecurity, AppState, SyncPointers),
                AutomergeSyncError,
            > {
                let profile = from_automerge(&profile_bytes)?;
                let security = from_automerge(&security_bytes)?;
                let app_state = from_automerge(&app_state_bytes)?;
                let pointers = from_automerge(&pointers_bytes)?;
                Ok((profile, security, app_state, pointers))
            },
        )
        .await
        .map_err(|_| AutomergeSyncError::WorkerPanicked)??;
        Ok(IdentityPull {
            profile,
            security,
            app_state,
            pointers,
            profile_doc,
            security_doc,
            app_state_doc,
        })
    })
    .await
}

/// One retry round of identity subkey reads: the first pending subkey is
/// read alone (a success seeds Veilid's per-record value-node cache for the
/// others), then the rest at unbounded concurrency — each read is one DHT
/// round trip, so the fan-out costs the slowest one, not a wave budget.
///
/// `newer` is the inspect verdict from [`inspect_newer_subkeys`]: subkeys in
/// the set are force-read from the network (the network copy is ahead of
/// this device's persisted local copy); subkeys outside it read from cache
/// (local is up to date — and a device with no local copy falls through to
/// the network anyway). `None` means the inspect failed, so every read is
/// forced: the inspect is an optimization, never a correctness gate.
async fn load_identity_round(
    key: &RecordKey,
    subkeys: &[(u32, &str)],
    pending: &[usize],
    secret: &SharedSecret,
    newer: Option<&ValueSubkeyRangeSet>,
) -> Vec<(usize, Result<Option<Bytes>>)> {
    let force = |i: usize| newer.is_none_or(|n| n.contains(subkeys[i].0));
    let mut results = Vec::with_capacity(pending.len());
    let Some((&first, rest)) = pending.split_first() else {
        return results;
    };
    results.push((
        first,
        store::load_structured(key, subkeys[first].0, secret, force(first)).await,
    ));
    // Unbounded over the remaining subkeys: each is one DHT read, and the
    // results keep input order so slot indices stay aligned.
    let rest_results = concurrency::map_concurrent(rest.iter().copied(), |i: usize| async move {
        let result: Result<Option<Bytes>> =
            store::load_structured(key, subkeys[i].0, secret, force(i)).await;
        (i, result)
    })
    .await;
    results.extend(rest_results);
    results
}

/// One `SyncGet` inspect over the identity subkeys, returning the set where
/// the network holds a newer sequence than this device's local record store.
/// `None` when the inspect fails — callers must then force-read everything,
/// because the local copy persists across restarts and a cached read would
/// silently return this device's own stale data.
async fn inspect_newer_subkeys(
    key: &RecordKey,
    subkeys: &[(u32, &str)],
) -> Option<ValueSubkeyRangeSet> {
    let range: ValueSubkeyRangeSet = subkeys.iter().map(|&(sk, _)| sk).collect();
    match store::inspect_record(key, Some(range), DHTReportScope::SyncGet).await {
        Ok(report) => Some(report.newer_online_subkeys()),
        Err(e) => {
            debug!("identity sync: freshness inspect failed, forcing all reads: {e:#}");
            None
        }
    }
}

/// Sorts one round's read results into loaded values and retryable errors.
/// An empty read is an error here: every identity subkey is written at
/// account creation, so "no value" means no replica was found — not a
/// legitimately absent value.
fn classify_round_results(
    round: usize,
    subkeys: &[(u32, &str); 4],
    results: Vec<(usize, Result<Option<Bytes>>)>,
    loaded: &mut [Option<Bytes>; 4],
    errors: &mut Vec<(usize, AnyhowError)>,
) {
    errors.clear();
    for (i, result) in results {
        match result {
            Ok(Some(bytes)) => loaded[i] = Some(bytes),
            Ok(None) => {
                debug!(
                    round,
                    subkey = subkeys[i].0,
                    name = subkeys[i].1,
                    "identity sync: no replica found"
                );
                // Not a network failure: the read succeeded and no replica
                // holds the subkey, so retries cannot produce it.
                errors.push((
                    i,
                    AnyhowError::new(store::DhtSubkeyMissing)
                        .context("no replica found on the network"),
                ));
            }
            Err(e) => {
                debug!(
                    round,
                    subkey = subkeys[i].0,
                    name = subkeys[i].1,
                    "identity sync: read failed: {e:#}"
                );
                errors.push((i, e));
            }
        }
    }
}

/// One `SyncGet` inspect fanout over the given identity subkeys, seeding
/// Veilid's per-record value-node cache for the value reads that follow.
/// Read-only: does not pull newer values or mutate local record state.
async fn seed_identity_value_nodes(key: &RecordKey, subkeys: &[(u32, &str)]) -> Result<()> {
    let range: ValueSubkeyRangeSet = subkeys.iter().map(|&(sk, _)| sk).collect();
    let _report = store::inspect_record(key, Some(range), DHTReportScope::SyncGet).await?;
    Ok(())
}

/// Re-pulls the identity record until its source completes onboarding or the
/// configured stale-source budget is spent.
///
/// Pairing-lane policy: a QR can only be minted by an onboarded identity, so
/// a pull missing onboarding data means the sharing device's post-onboarding
/// publish is still in flight (e.g. stuck on a hostile network). Admitting
/// that copy would pop the onboarding wizard and fork history — retry with
/// waits until the complete copy lands. Returns `None` when the budget was
/// spent without onboarded data; transient pull failures keep the previous
/// freshest pull and only log.
pub(in crate::user) async fn repull_until_onboarded(
    dht_key: &RecordKey,
    password_secret: &SharedSecret,
    fresh: IdentityPull,
) -> Option<IdentityPull> {
    let mut pull = fresh;
    for wait_ms in STALE_SOURCE_REPULL_WAITS_MS {
        sleep_ms(wait_ms).await;
        match sync_from_dht_for_pairing(dht_key, password_secret).await {
            Ok(fresh) => pull = fresh,
            Err(e) => warn!("stale-source re-pull failed: {e:#}"),
        }
        if pull.is_onboarded() {
            return Some(pull);
        }
    }
    pull.is_onboarded().then_some(pull)
}
