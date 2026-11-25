//! Decode-validate-commit engine for restores: builds a fully checked plan
//! from decrypted backup sections and applies it in Replace or Merge mode.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};

use crate::{
    chat::{contact::ContactList, conversation::ConversationIndex, storage::import_all_raw},
    models::totp::TotpTokenList,
    storage::{
        backup,
        store_trait::{CrdtModel, SyncSurface, decode_snapshot, replace_local},
    },
    sync::mutate::{MutationResult, mutate},
    user::ctx::UserCtx,
};

use super::shared::BackupSections;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RestoreMode {
    Replace,
    Merge,
}

pub(super) trait BackupMerge: CrdtModel {
    fn merge_backup(&mut self, backup: &Self);
}

fn add_missing<T: Clone>(current: &mut HashMap<String, T>, backup: &HashMap<String, T>) {
    for (key, value) in backup {
        current.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

macro_rules! impl_backup_merge {
    ($($ty:ty => $field:ident),+ $(,)?) => {
        $(
            impl BackupMerge for $ty {
                fn merge_backup(&mut self, backup: &Self) {
                    add_missing(&mut self.$field, &backup.$field);
                    self.last_updated = self.last_updated.max(backup.last_updated);
                }
            }
        )+
    };
}

impl_backup_merge!(
    TotpTokenList => tokens,
    ContactList => contacts,
    ConversationIndex => conversations,
);

/// Everything a restore will apply — fully decoded and cross-checked before
/// any state mutates. A malformed or mislabeled backup fails here, with the
/// local store untouched.
#[derive(Default)]
pub(super) struct RestorePlan {
    pub(super) identity: Option<backup::IdentityBackup>,
    pub(super) totp: Option<TotpTokenList>,
    pub(super) contacts: Option<ContactList>,
    pub(super) index: Option<ConversationIndex>,
    pub(super) conversations: Option<Vec<(String, Vec<u8>)>>,
}

/// Decode every section up front and enforce structural invariants:
/// unknown/duplicate sections fail, and conversation data may only travel
/// with the index it must stay coherent with.
pub(super) fn build_plan(sections: &BackupSections) -> Result<RestorePlan> {
    let mut plan = RestorePlan::default();
    for (model_id, data) in sections {
        let duplicate = match model_id.as_str() {
            backup::IDENTITY_SECTION_ID => plan
                .identity
                .replace(backup::IdentityBackup::decode(data).context("identity section")?)
                .is_some(),
            TotpTokenList::ID => {
                let value = decode_snapshot::<TotpTokenList>(data).context("totp section")?;
                plan.totp.replace(value).is_some()
            }
            ContactList::ID => {
                let value = decode_snapshot::<ContactList>(data).context("contacts section")?;
                plan.contacts.replace(value).is_some()
            }
            ConversationIndex::ID => {
                let value =
                    decode_snapshot::<ConversationIndex>(data).context("conversations section")?;
                plan.index.replace(value).is_some()
            }
            backup::CONVERSATIONS_DATA_SECTION_ID => {
                let items = backup::decode_kv_list(data).context("conversations_data section")?;
                // Row-level Cap'n Proto validation runs inside `import_all_raw`
                // on `spawn_blocking` before any lock or write takes place.
                plan.conversations.replace(items).is_some()
            }
            other => bail!("Unknown section: {other}"),
        };
        ensure!(!duplicate, "Backup has duplicate section: {model_id}");
    }
    ensure!(
        plan.conversations.is_none() || plan.index.is_some(),
        "Backup contains conversation data but no conversation index"
    );
    Ok(plan)
}

/// Strict account binding — runs before **any** section is applied.
///
/// A backup carrying an identity section may only restore into the session it
/// was exported from: username and account ID must be non-empty and match
/// exactly (blank or malformed fields fail closed). Partial exports
/// (TOTP/contacts/index) carry no identity section and restore into the
/// current account — but account-scoped conversation data must never appear
/// without one.
///
/// The DHT key binds only when the backup carries one. It is the identity
/// record's own key, minted when onboarding creates that record, so an export
/// taken by this very session while the record was still being created writes
/// a blank one — as does an import into a session that has not minted its own
/// yet. Requiring it non-empty refused the account its own backup (silently:
/// the restore simply applied nothing), while username + account ID — both set
/// at signup, before any network work — already identify the account, so a
/// blank one cannot admit another account's backup.
pub(super) fn verify_account_binding(ctx: &UserCtx, plan: &RestorePlan) -> Result<()> {
    let Some(identity) = &plan.identity else {
        ensure!(
            plan.conversations.is_none(),
            "Backup contains conversation data but no identity section"
        );
        return Ok(());
    };

    let matches = |field: &str, expected: &str| !field.is_empty() && field == expected;
    ensure!(
        matches(&identity.username, &ctx.username)
            && matches(&identity.account_id, &ctx.account_id)
            && (identity.dht_key.is_empty() || identity.dht_key == *ctx.dht_key),
        "This backup belongs to a different account"
    );
    Ok(())
}

/// Apply a small CRDT model (TOTP, contacts) and enqueue it for DHT sync.
///
/// - `Replace`: [`replace_local`] — snapshot plus the restored value
///   reconciled into the automerge doc and its currentness marker, all in one
///   transaction, so entries absent from the backup are deleted by the CRDT
///   and stay deleted on the next sync.
/// - `Merge`: union stable item keys under the account lock. Existing items
///   retain all scalar fields; only missing backup items are added.
pub(super) async fn apply_model<T: BackupMerge>(
    ctx: &UserCtx,
    value: &T,
    mode: RestoreMode,
) -> Result<()> {
    match mode {
        RestoreMode::Replace => replace_local::<T>(ctx.storage_key(), value).await?,
        RestoreMode::Merge => {
            return mutate::<T, _>(ctx, |current| {
                current.merge_backup(value);
                Ok(MutationResult::modified(()))
            })
            .await;
        }
    }
    ctx.enqueue_touch(T::OP_TYPE).await?;
    Ok(())
}

/// Restore the conversation index and per-conversation message data as one
/// coherent unit.
///
/// - `Replace` with data present (full backup): index snapshot + all message
///   rows commit in **one `TableDB` transaction** ([`import_all_raw`] — they
///   share a table), so the index can never point at rows that don't exist.
/// - `Replace`, index only (partial export): the index is replaced; local
///   message rows are kept — the backup doesn't carry them, wiping would be
///   data loss.
/// - `Merge`: union index keys, preserving existing entries; only missing
///   message rows are added.
///
/// [`import_all_raw`]: crate::chat::storage::import_all_raw
pub(super) async fn apply_conversations(
    ctx: &UserCtx,
    index: Option<&ConversationIndex>,
    items: Option<&[(String, Vec<u8>)]>,
    mode: RestoreMode,
) -> Result<()> {
    if index.is_none() && items.is_none() {
        return Ok(());
    }
    let account_id: &str = ctx.account_id.as_ref();
    match (mode, index, items) {
        (RestoreMode::Replace, Some(index), Some(items)) => {
            let packed = index.pack()?;
            import_all_raw(account_id, items, Some(&packed), true).await?;
        }
        (RestoreMode::Replace, Some(index), None) => {
            replace_local::<ConversationIndex>(ctx.storage_key(), index).await?;
        }
        (RestoreMode::Merge, _, _) => {
            if let Some(index) = index {
                apply_model(ctx, index, RestoreMode::Merge).await?;
            }
            if let Some(items) = items {
                import_all_raw(account_id, items, None, false).await?;
            }
            return Ok(());
        }
        (RestoreMode::Replace, None, _) => {
            bail!("Cannot restore conversation data without an index");
        }
    }
    ctx.enqueue_touch(ConversationIndex::OP_TYPE).await?;
    Ok(())
}
