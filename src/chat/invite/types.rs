//! Pending-invite types and shared state: the UI signal plus the deferred
//! queue holding invites received before a usable session exists.

use std::sync::{LazyLock, Mutex, MutexGuard};

use dioxus::prelude::*;

pub use crate::chat::storage::StoredPendingInvite as PendingInvite;
pub(super) use crate::chat::storage::pending_invite_is_recent as invite_is_recent;
use crate::{
    chat::storage::MAX_PENDING_INVITES,
    helpers::{async_utils::now_epoch_millis, store_lock::recover_poison},
};

const MAX_DEFERRED_PENDING_INVITES: usize = 128;

/// Pending invites from unknown senders, awaiting user accept/reject.
pub static PENDING_INVITES: GlobalSignal<Vec<PendingInvite>> = Signal::global(Vec::new);

#[derive(Clone)]
pub(super) struct DeferredPendingInvite {
    pub(super) account_id: Option<String>,
    pub(super) invite: PendingInvite,
}

static DEFERRED_PENDING_INVITES: LazyLock<Mutex<Vec<DeferredPendingInvite>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

pub(super) fn deferred_pending_invites() -> MutexGuard<'static, Vec<DeferredPendingInvite>> {
    recover_poison(DEFERRED_PENDING_INVITES.lock())
}

fn prune_expired<T>(items: &mut Vec<T>, now: u64, received_at: fn(&T) -> u64) {
    items.retain(|item| invite_is_recent(received_at(item), now));
}

fn sort_and_cap<T>(items: &mut Vec<T>, cap: usize, received_at: fn(&T) -> u64) {
    let (kept, excess) =
        crate::chat::storage::sort_and_cap(std::mem::take(items), cap, received_at);
    *items = kept;
    drop(excess);
}

fn insert_deduped(invites: &mut Vec<PendingInvite>, invite: PendingInvite) {
    let now = now_epoch_millis();
    prune_expired(invites, now, |p| p.received_at);
    if !invite_is_recent(invite.received_at, now)
        || invites
            .iter()
            .any(|pending| pending.manifest_key_str == invite.manifest_key_str)
    {
        return;
    }
    invites.push(invite);
    sort_and_cap(invites, MAX_PENDING_INVITES, |p| p.received_at);
}

pub(super) fn insert_pending_invite_signal(invite: PendingInvite) {
    PENDING_INVITES.with_mut(|invites| {
        insert_deduped(invites, invite);
    });
}

pub(super) fn remove_pending_invite_signal(manifest_key_str: &str) {
    PENDING_INVITES.with_mut(|invites| {
        invites.retain(|pending| pending.manifest_key_str != *manifest_key_str);
    });
}

pub(super) fn pending_invite_snapshot(manifest_key_str: &str) -> Option<PendingInvite> {
    let now = now_epoch_millis();
    PENDING_INVITES.with(|invites| {
        invites
            .iter()
            .find(|pending| {
                pending.manifest_key_str == *manifest_key_str
                    && invite_is_recent(pending.received_at, now)
            })
            .cloned()
    })
}
pub(super) fn push_deferred_pending_invite(account_id: Option<String>, invite: PendingInvite) {
    restore_deferred_pending_invites(vec![DeferredPendingInvite { account_id, invite }]);
}

fn merge_deferred_invites(
    invites: &mut Vec<DeferredPendingInvite>,
    retained: Vec<DeferredPendingInvite>,
    now: u64,
) {
    prune_expired(invites, now, |d| d.invite.received_at);
    for pending in retained {
        if !invites.iter().any(|existing| {
            existing.account_id == pending.account_id
                && existing.invite.manifest_key_str == pending.invite.manifest_key_str
        }) {
            invites.push(pending);
        }
    }
    sort_and_cap(invites, MAX_DEFERRED_PENDING_INVITES, |d| {
        d.invite.received_at
    });
}

pub(super) fn restore_deferred_pending_invites(mut retained: Vec<DeferredPendingInvite>) {
    let now = now_epoch_millis();
    prune_expired(&mut retained, now, |d| d.invite.received_at);
    if retained.is_empty() {
        prune_expired(&mut deferred_pending_invites(), now, |d| {
            d.invite.received_at
        });
        return;
    }
    merge_deferred_invites(&mut deferred_pending_invites(), retained, now);
}

impl PendingInvite {
    pub(in crate::chat) fn purge_deferred_for_account(account_id: &str) {
        deferred_pending_invites()
            .retain(|pending| pending.account_id.as_deref() != Some(account_id));
    }
}
