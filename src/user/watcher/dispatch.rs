//! `ValueChange` entry routing: conversation watches, child records, and the
//! identity record itself.

use std::sync::Arc;

use dioxus::prelude::ReadableExt;
use veilid_core::VeilidValueChange;

use crate::{
    chat::{
        manager::is_watched_conversation, signal::evict_routes_for_identity,
        watcher as chat_watcher,
    },
    platform::lifecycle::note_sync_activity,
    session::{epoch as session, phase::SESSION},
    user::{self, identity_layout as layout},
    veilid::store,
};

use super::{
    child_pull::pull_child_for_feature,
    resync::{ResyncScope, resync_identity, resync_with_scope},
    shared::dict_only_sync,
};

pub async fn handle_dht_change(key: Arc<str>, change: Box<VeilidValueChange>) {
    note_sync_activity();

    if is_watched_conversation(&key) {
        let pushed = store::pushed_value(&change);
        chat_watcher::handle_conversation_change(
            key,
            change.subkeys,
            pushed,
            session::SessionGuard::capture(),
        )
        .await;
        return;
    }

    if let Some(feature) = user::child_watch::feature_for_key(&key) {
        handle_child_change(feature, &change).await;
        return;
    }

    let is_own_identity = SESSION
        .peek()
        .authenticated_user()
        .is_some_and(|user| !user.dht_key().is_empty() && user.dht_key() == key.as_ref());
    if is_own_identity {
        Box::pin(handle_identity_change(change, &key)).await;
    }
}

/// Handles a `ValueChange` on a watched child record: re-arms a dead watch
/// (`count == 0`) and pulls just that feature's model from the network.
async fn handle_child_change(feature: &'static str, change: &VeilidValueChange) {
    if change.count == 0 {
        user::child_watch::sync().await;
    }
    pull_child_for_feature(feature).await;
}

/// Routes an identity-record `ValueChange` by changed subkey so a pairing
/// beacon or route update never triggers a full identity resync.
async fn handle_identity_change(change: Box<VeilidValueChange>, change_key: &str) {
    // `count == 0` signals the watch died (expired or cancelled upstream):
    // re-arm it. With an empty subkey set it is a pure death notice — the
    // record may have changed while unwatched, so pull everything. A final
    // real change can ride along with `count == 0`; dispatch it normally.
    if change.count == 0 {
        user::identity_watch::reestablish().await;
        if change.subkeys.is_empty() {
            Box::pin(resync_identity()).await;
            return;
        }
    } else if change.subkeys.is_empty() {
        // Unknown scope (forced resync) — pull everything.
        Box::pin(resync_identity()).await;
        return;
    }

    // Pairing beacon: publish to the share-mode signal; not a data change.
    // Veilid attaches the delivered value to the *first* changed subkey
    // only, so for any other leading subkey the beacon must be read back
    // from the network (handle_watch_change falls back on `None`).
    if change.subkeys.contains(layout::SUBKEY_PAIRING_BEACON) {
        let raw = (change.subkeys.first() == Some(layout::SUBKEY_PAIRING_BEACON))
            .then(|| change.value.as_ref().map(|v| v.data().to_vec()))
            .flatten();
        user::pairing_beacon::handle_watch_change(change.key.clone(), raw).await;
    }

    // Routes: another of this account's devices republished its private
    // route. Evict any cached imported route for this identity so the next
    // signal send re-imports the fresh blob instead of riding the TTL.
    if change.subkeys.contains(layout::SUBKEY_ROUTES) {
        evict_routes_for_identity(change_key).await;
    }

    let core_changed = [
        layout::SUBKEY_PROFILE,
        layout::SUBKEY_SECURITY,
        layout::SUBKEY_APP_STATE,
        layout::SUBKEY_POINTERS,
    ]
    .into_iter()
    .any(|s| change.subkeys.contains(s));

    let dict_changed = [layout::SUBKEY_DICT_META, layout::SUBKEY_DICT_BLOB]
        .into_iter()
        .any(|s| change.subkeys.contains(s));

    if core_changed {
        // Scoped: a pointers hint means child records may have appeared or
        // migrated — pull them and re-arm their watches. Any other core
        // hint (profile/security/app-state) resyncs the core subkeys only;
        // the freshness inspect inside the pull keeps unchanged subkeys as
        // local cache reads, so a theme change no longer fans out into
        // three child pulls and a dictionary check.
        let scope = if change.subkeys.contains(layout::SUBKEY_POINTERS) {
            ResyncScope::Children
        } else {
            ResyncScope::Core
        };
        Box::pin(resync_with_scope(scope)).await;
    }
    // Dict hints run directly (not via the resync): a coalesced multi-subkey
    // event can carry core and dict subkeys in one notification.
    if dict_changed {
        dict_only_sync().await;
    }
    // The keypair blob (3) needs no action here: it is only read at login.
}
