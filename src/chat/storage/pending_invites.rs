//! Per-account `TableDB` rows for received-but-unaccepted invites: store, load,
//! delete single/all.

use std::str;

use anyhow::{Context, Result, anyhow, bail};
use veilid_core::RecordKey;

use crate::{
    chat::session_flow::purge_canceled_contact_work_for_account,
    helpers::async_utils::now_epoch_millis,
    storage::{account_table::AccountTable, background_errors::report_nonfatal},
    veilid::core::parse_record_key,
};

pub(in crate::chat) const MAX_PENDING_INVITES: usize = 64;

/// Sorts `items` by `received_at` and caps to `cap`, returning the kept items
/// and the drained excess (oldest first). Shared by the invite signal and the
/// stored-invite prune so both apply exactly the same rule.
pub(in crate::chat) fn sort_and_cap<T>(
    mut items: Vec<T>,
    cap: usize,
    received_at: fn(&T) -> u64,
) -> (Vec<T>, Vec<T>) {
    items.sort_by_key(received_at);
    let excess = items.len().saturating_sub(cap);
    let excess_items = if excess > 0 {
        items.drain(..excess).collect()
    } else {
        Vec::new()
    };
    (items, excess_items)
}
const PENDING_INVITE_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

pub(in crate::chat) const fn pending_invite_is_recent(received_at: u64, now: u64) -> bool {
    received_at <= now && now - received_at <= PENDING_INVITE_TTL_MS
}

/// Rows keyed `{account_id}\0{manifest_key}` → `{inviter_key}\0{received_at}`.
const TABLE: AccountTable = AccountTable::new("po_pending_invites");

#[derive(Debug, Clone)]
pub struct StoredPendingInvite {
    pub manifest_key: RecordKey,
    pub manifest_key_str: String,
    pub inviter_key: RecordKey,
    pub received_at: u64,
}

fn encode_value(inviter_key: &RecordKey, received_at: u64) -> Vec<u8> {
    let inviter_key_str = inviter_key.to_string();
    let received_at_str = received_at.to_string();
    let mut value = Vec::with_capacity(inviter_key_str.len() + received_at_str.len() + 1);
    value.extend_from_slice(inviter_key_str.as_bytes());
    value.push(0);
    value.extend_from_slice(received_at_str.as_bytes());
    value
}

fn decode_value(bytes: &[u8], manifest_key_str: &str) -> Result<(RecordKey, u64)> {
    let Some(split) = bytes.iter().position(|byte| *byte == 0) else {
        bail!("missing invite separator for {manifest_key_str}");
    };
    let inviter_key_str = str::from_utf8(&bytes[..split])
        .context("pending invite inviter key was not valid UTF-8")?;
    let inviter_key = parse_record_key(inviter_key_str)
        .map_err(|e| anyhow!("invalid inviter key for {manifest_key_str}: {e}"))?;
    let received_at = str::from_utf8(&bytes[split + 1..])
        .context("pending invite timestamp was not valid UTF-8")?
        .parse::<u64>()
        .context("pending invite timestamp was not a valid integer")?;
    Ok((inviter_key, received_at))
}

pub async fn store_pending_invite_for_account(
    account_id: &str,
    manifest_key_str: &str,
    inviter_key: &RecordKey,
    received_at: u64,
) -> Result<()> {
    let now = now_epoch_millis();
    if !pending_invite_is_recent(received_at, now) {
        bail!("pending invite is expired");
    }
    let invites = load_pending_invites_for_account(account_id).await?;
    if invites
        .iter()
        .any(|invite| invite.manifest_key_str == manifest_key_str)
    {
        return Ok(());
    }
    if invites.len() >= MAX_PENDING_INVITES {
        bail!("pending invite limit reached");
    }
    TABLE
        .store_if_absent(
            account_id,
            manifest_key_str.as_bytes(),
            &encode_value(inviter_key, received_at),
        )
        .await
        .map(|_stored| ())
}

pub async fn delete_pending_invite_for_account(
    account_id: &str,
    manifest_key_str: &str,
) -> Result<()> {
    TABLE.delete(account_id, manifest_key_str.as_bytes()).await
}

pub async fn load_pending_invites_for_account(
    account_id: &str,
) -> Result<Vec<StoredPendingInvite>> {
    let now = now_epoch_millis();
    let mut invites = Vec::new();

    for (suffix, value) in TABLE.entries(account_id).await? {
        let manifest_key_str = match str::from_utf8(&suffix) {
            Ok(key_str) if !key_str.is_empty() => key_str.to_owned(),
            Ok(_) => {
                report_nonfatal(
                    "chat.storage",
                    "Pending invite row had an empty manifest key",
                    format!("account_id={account_id}"),
                );
                TABLE.delete(account_id, &suffix).await?;
                continue;
            }
            Err(e) => {
                report_nonfatal(
                    "chat.storage",
                    "Pending invite manifest key was not valid UTF-8",
                    format!("account_id={account_id}: {e}"),
                );
                TABLE.delete(account_id, &suffix).await?;
                continue;
            }
        };
        let manifest_key = match parse_record_key(&manifest_key_str) {
            Ok(key) => key,
            Err(e) => {
                report_nonfatal(
                    "chat.storage",
                    "Pending invite manifest key was invalid",
                    format!("account_id={account_id} manifest_key={manifest_key_str}: {e}"),
                );
                TABLE.delete(account_id, &suffix).await?;
                continue;
            }
        };
        let (inviter_key, received_at) = match decode_value(&value, &manifest_key_str) {
            Ok(decoded) => decoded,
            Err(e) => {
                report_nonfatal(
                    "chat.storage",
                    "Failed to decode pending invite row",
                    format!("account_id={account_id} manifest_key={manifest_key_str}: {e}"),
                );
                TABLE.delete(account_id, &suffix).await?;
                continue;
            }
        };
        if !pending_invite_is_recent(received_at, now) {
            TABLE
                .delete(account_id, manifest_key_str.as_bytes())
                .await?;
            continue;
        }
        invites.push(StoredPendingInvite {
            manifest_key,
            manifest_key_str,
            inviter_key,
            received_at,
        });
    }

    // Sort-and-cap in one shared helper (same rule the invite signal uses);
    // the cap returns the drained excess, which is deleted from the table.
    let (kept, excess) = sort_and_cap(invites, MAX_PENDING_INVITES, |invite| invite.received_at);
    for invite in excess {
        TABLE
            .delete(account_id, invite.manifest_key_str.as_bytes())
            .await?;
    }
    Ok(kept)
}

pub async fn delete_all_pending_invites_for_account(account_id: &str) -> Result<()> {
    StoredPendingInvite::purge_deferred_for_account(account_id);
    purge_canceled_contact_work_for_account(account_id);
    TABLE.delete_all_for_account(account_id).await
}
