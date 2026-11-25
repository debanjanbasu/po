//! Owner/member watch-update types and loaders: subkey-range → member-change
//! mapping, parallel owner/member subkey fetches with nonfatal automerge decoding.

use std::collections::HashMap;

use anyhow::{Error, Result};
use autosurgeon::Hydrate;
use bytes::Bytes;
use tokio::try_join;
use veilid_core::{KeyPair, RecordKey, SharedSecret, ValueSubkeyRangeSet};

use crate::{
    chat::{
        conv_keys::verify_roster_binding,
        feed::{MessagePage, ParticipantFeedMeta},
        manifest::{ConversationKeyRing, ConversationManifest, ConversationRoster},
        record,
    },
    helpers::{async_utils::retry_once_on_transient, concurrency::map_concurrent, spawn_blocking},
    storage::background_errors::report_nonfatal,
    veilid::store::{self, PushedValue, StoreMode},
};

type MemberLoadResult = (usize, Result<(Option<Bytes>, Option<Bytes>), Error>);

#[derive(Clone, Default)]
pub(super) struct OwnerUpdates {
    pub(super) manifest: Option<ConversationManifest>,
    pub(super) roster: Option<ConversationRoster>,
    pub(super) key_ring: Option<ConversationKeyRing>,
}

pub(super) struct MemberRemoteUpdate {
    pub(super) member_index: usize,
    pub(super) feed_meta: Option<ParticipantFeedMeta>,
    pub(super) page: Option<MessagePage>,
}

pub(super) fn member_changes_for_subkeys(
    changed_subkeys: &ValueSubkeyRangeSet,
) -> HashMap<usize, bool> {
    let mut changes = HashMap::<usize, bool>::new();
    for subkey in changed_subkeys.iter() {
        let Some(member_index) = record::member_index_for_subkey(subkey) else {
            continue;
        };
        *changes.entry(member_index).or_default() |=
            subkey == record::member_feed_subkey(member_index);
    }
    changes
}

/// Decode an optional automerge payload, reporting failures as nonfatal.
/// Thin local shim over the shared `chat::record::decode_automerge` wrapper,
/// keeping this module's `chat.watcher` component tag on the report.
fn decode_automerge<T: Hydrate>(
    raw: Option<&[u8]>,
    label: &'static str,
    context: &str,
) -> Option<T> {
    record::decode_automerge(raw, "chat.watcher", label, context)
}

/// Owner-phase doc fetch; runs inside the caller's `with_record` scope. Each
/// load is served from the change's delivered value when that value is the
/// subkey's, and read back from the network otherwise.
async fn load_owner_docs(
    key: &RecordKey,
    secret: &SharedSecret,
    changed: OwnerFlags,
    pushed: Option<&PushedValue>,
) -> Result<(Option<Bytes>, Option<Bytes>, Option<Bytes>)> {
    let (manifest, roster, key_ring) = try_join!(
        async {
            if changed.manifest {
                store::load_opt_preferring_push(
                    key,
                    ConversationManifest::SUBKEY,
                    Some(secret),
                    StoreMode::StructuredEncrypted,
                    pushed,
                )
                .await
            } else {
                Ok(None)
            }
        },
        async {
            if changed.roster {
                store::load_opt_preferring_push(
                    key,
                    ConversationRoster::SUBKEY,
                    Some(secret),
                    StoreMode::StructuredEncrypted,
                    pushed,
                )
                .await
            } else {
                Ok(None)
            }
        },
        async {
            if changed.key_ring {
                store::load_opt_preferring_push(
                    key,
                    ConversationKeyRing::SUBKEY,
                    None,
                    StoreMode::Unencrypted,
                    pushed,
                )
                .await
            } else {
                Ok(None)
            }
        },
    )?;
    Ok((manifest, roster, key_ring))
}

/// Decodes owner docs fetched in the caller's `with_record` scope.
async fn decode_owner_updates(
    loaded: (Option<Bytes>, Option<Bytes>, Option<Bytes>),
    manifest_key_str: &str,
) -> OwnerUpdates {
    let key_str = manifest_key_str.to_owned();
    spawn_blocking(move || OwnerUpdates {
        manifest: decode_automerge(
            loaded.0.as_deref(),
            "Failed to decode remote manifest",
            &key_str,
        ),
        roster: decode_automerge(
            loaded.1.as_deref(),
            "Failed to decode remote roster",
            &key_str,
        ),
        key_ring: decode_automerge(
            loaded.2.as_deref(),
            "Failed to decode remote key ring",
            &key_str,
        ),
    })
    .await
    .unwrap_or_else(|e| {
        report_nonfatal(
            "chat.watcher",
            "Owner update decode worker failed",
            format!("{manifest_key_str}: {e}"),
        );
        OwnerUpdates::default()
    })
}

/// Member-phase doc fetch; runs inside the caller's `with_record` scope.
/// Owns `key`/`secret` clones because the per-member task needs them for its
/// whole DHT read.
///
/// Each member's read is wrapped in one `retry_once_on_transient`, which used
/// to sleep the whole 2 s wait inside the record's lock on a transient miss;
/// the scope-level retry (see `with_record`) now owns that, so the per-member
/// wrapper classifies but no longer waits on a transient error.
async fn load_member_results(
    key: &RecordKey,
    ordered_changes: &[(usize, bool)],
    secret: &SharedSecret,
    pushed: Option<&PushedValue>,
) -> Result<Vec<MemberLoadResult>> {
    let key = key.clone();
    let secret = secret.clone();
    let pushed = pushed.cloned();
    Ok::<_, Error>(
        map_concurrent(
            ordered_changes.iter().copied(),
            // Unbounded: each member's read is one DHT round trip, and the
            // results come back in input order so `MemberLoadResult` slot
            // indices stay aligned without a permit pool.
            //
            // Clone the scope-owned handles into each task: the closure must
            // own its data (the scope's borrows don't outlive it).
            // `PushedValue` is a `Bytes` handle, so this is a refcount bump.
            async move |(member_index, feed_meta_changed)| {
                let key = key.clone();
                let secret = secret.clone();
                let pushed = pushed.clone();
                let load = move || {
                    let key = key.clone();
                    let secret = secret.clone();
                    let pushed = pushed.clone();
                    async move {
                        let feed_meta = if feed_meta_changed {
                            store::load_opt_preferring_push(
                                &key,
                                record::member_feed_subkey(member_index),
                                Some(&secret),
                                StoreMode::StructuredEncrypted,
                                pushed.as_ref(),
                            )
                            .await
                        } else {
                            Ok(None)
                        };
                        let page_raw = store::load_opt_preferring_push(
                            &key,
                            record::member_page_subkey(member_index),
                            Some(&secret),
                            StoreMode::StructuredEncrypted,
                            pushed.as_ref(),
                        )
                        .await;
                        feed_meta
                            .and_then(|feed_meta| page_raw.map(|page_raw| (feed_meta, page_raw)))
                    }
                };
                (
                    member_index,
                    retry_once_on_transient(store::is_transient_network_error, load).await,
                )
            },
        )
        .await,
    )
}

pub(super) struct CombinedUpdates {
    pub(super) owner: OwnerUpdates,
    pub(super) member: Vec<MemberRemoteUpdate>,
}

/// Owner/member doc fetch inside the single record scope; each phase retries
/// once on transient errors (previous loader semantics).
async fn fetch_owner_and_member(
    key: &RecordKey,
    secret: &SharedSecret,
    flags: OwnerFlags,
    ordered_changes: &[(usize, bool)],
    pushed: Option<&PushedValue>,
) -> Result<(
    Result<(Option<Bytes>, Option<Bytes>, Option<Bytes>)>,
    Result<Vec<MemberLoadResult>>,
)> {
    // Each phase classifies its own transient errors (previous loader
    // semantics); the 2 s wait a transient error costs now happens outside the
    // record scope, on `with_record`'s own retry.
    let owner = if flags.any() {
        retry_once_on_transient(store::is_transient_network_error, || {
            load_owner_docs(key, secret, flags, pushed)
        })
        .await
    } else {
        Ok((None, None, None))
    };

    let member = if ordered_changes.is_empty() {
        Ok(Vec::new())
    } else {
        retry_once_on_transient(store::is_transient_network_error, || {
            load_member_results(key, ordered_changes, secret, pushed)
        })
        .await
    };

    Ok((owner, member))
}

/// Decodes member load results in `spawn_blocking`, sorted by member index.
async fn decode_member_updates(
    member: Result<Vec<MemberLoadResult>>,
    manifest_key_str: &str,
) -> Vec<MemberRemoteUpdate> {
    let results = match member {
        Ok(results) => results,
        Err(error) => {
            store::report_nonfatal_unless_network(
                "chat.watcher",
                "Failed to load member subkeys from DHT",
                &error,
            );
            return Vec::new();
        }
    };
    let key_str = manifest_key_str.to_owned();
    spawn_blocking(move || {
        let mut updates = collect_member_updates(results, &key_str);
        updates.sort_unstable_by_key(|update| update.member_index);
        updates
    })
    .await
    .unwrap_or_else(|e| {
        report_nonfatal(
            "chat.watcher",
            "Member update decode worker failed",
            format!("{manifest_key_str}: {e}"),
        );
        Vec::new()
    })
}

/// Which owner subkeys a change announced, already narrowed to this
/// conversation's subkeys. Grouped so the shared loader takes it as one value.
#[derive(Clone, Copy, Default)]
pub(super) struct OwnerFlags {
    manifest: bool,
    roster: bool,
    key_ring: bool,
}

impl OwnerFlags {
    /// Which of the three owner subkeys a change announced.
    pub(super) fn from_subkeys(subkeys: &ValueSubkeyRangeSet) -> Self {
        Self {
            manifest: subkeys.contains(ConversationManifest::SUBKEY),
            roster: subkeys.contains(ConversationRoster::SUBKEY),
            key_ring: subkeys.contains(ConversationKeyRing::SUBKEY),
        }
    }
}

impl OwnerFlags {
    const fn any(self) -> bool {
        self.manifest || self.roster || self.key_ring
    }
}

/// Loads owner and member subkeys in ONE `with_record` scope on the
/// conversation record. Deadlock audit: the scope holds only the per-record
/// `RECORD_LOCKS` guard; `load_structured`/`load_unencrypted` inside it use
/// the scope's open record via `get_dht_value` and take no other locks, and
/// Fan-out inside a scope is unbounded. Merging the
/// former two scopes saves a second lock acquisition + timeout window per
/// change event.
///
/// A transient network error surfaces out of the scope and is retried by
/// `with_record` itself, after the lock is released — not slept out inside it.
///
/// `pushed` is the value the `ValueChange` delivered: the subkey it names is
/// decoded from the event, every other changed subkey is read back. That is
/// the hot path for a message arriving — the bytes are in hand and a forced
/// network read of the whole member page would be pure overhead.
pub(super) async fn load_conversation_updates(
    manifest_key: &RecordKey,
    manifest_key_str: &str,
    secret: &SharedSecret,
    owner: OwnerFlags,
    member_changes: &HashMap<usize, bool>,
    pushed: Option<PushedValue>,
) -> CombinedUpdates {
    let mut ordered_changes: Vec<_> = member_changes
        .iter()
        .map(|(member_index, change)| (*member_index, *change))
        .collect();
    ordered_changes.sort_unstable_by_key(|(member_index, _)| *member_index);

    let secret = secret.clone();
    let ordered = ordered_changes.clone();
    let loaded = store::with_record(manifest_key, None::<&KeyPair>, move |key| {
        let secret = secret.clone();
        async move { fetch_owner_and_member(&key, &secret, owner, &ordered, pushed.as_ref()).await }
    })
    .await;

    let (owner, member) = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            store::report_nonfatal_unless_network(
                "chat.watcher",
                "Failed to load conversation subkeys from DHT",
                &error,
            );
            (Ok((None, None, None)), Ok(Vec::new()))
        }
    };

    // Decode outside the record scope, in spawn_blocking, as before.
    let owner = match owner {
        Ok(loaded) => decode_owner_updates(loaded, manifest_key_str).await,
        Err(error) => {
            store::report_nonfatal_unless_network(
                "chat.watcher",
                "Failed to load owner subkeys from DHT",
                &error,
            );
            OwnerUpdates::default()
        }
    };
    // A remote roster is applied only if the record's writer slots back every
    // entry it names (see `verify_roster_binding`); one that does not is an
    // impersonation, so the whole owner set — same writer, same update — is
    // dropped and the verified local roster keeps attributing member pages.
    let roster_backed = match owner.roster.as_ref() {
        Some(roster) => match verify_roster_binding(manifest_key, roster).await {
            Ok(()) => true,
            Err(error) => {
                report_nonfatal(
                    "chat.watcher",
                    "Rejected a remote roster its record's writer slots do not back",
                    format!("{manifest_key_str}: {error}"),
                );
                false
            }
        },
        None => true,
    };
    let owner = if roster_backed {
        owner
    } else {
        OwnerUpdates::default()
    };
    let member = decode_member_updates(member, manifest_key_str).await;

    CombinedUpdates { owner, member }
}

fn collect_member_updates(
    results: Vec<MemberLoadResult>,
    manifest_key_str: &str,
) -> Vec<MemberRemoteUpdate> {
    let mut updates = Vec::with_capacity(results.len());
    for (member_index, result) in results {
        match result {
            Ok((feed_meta_raw, page_raw)) => {
                let ctx = format!("{manifest_key_str}:{member_index}");
                updates.push(MemberRemoteUpdate {
                    member_index,
                    feed_meta: decode_automerge(
                        feed_meta_raw.as_deref(),
                        "Failed to decode remote feed meta",
                        &ctx,
                    ),
                    page: decode_automerge(
                        page_raw.as_deref(),
                        "Failed to decode remote page (skipped)",
                        &ctx,
                    ),
                });
            }
            Err(error) => {
                store::report_nonfatal_unless_network(
                    "chat.watcher",
                    "Failed to load member subkeys from DHT",
                    &error,
                );
            }
        }
    }
    updates
}
