//! Locally persisted automerge docs for identity subkeys (profile/security).
//!
//! Gives the identity subkeys the same symmetric CRDT semantics as the
//! child records: pulls merge remote history into local history instead of
//! overwriting, and publishes write local history merged with the network
//! copy. On unstable networks a failed publish can therefore never be
//! undone by a later pull of the stale network copy.
//!
//! The `User` blob in `oo_users` remains the local snapshot of record;
//! these docs carry the CRDT *history* of its synced fields.
//!
//! Doc-byte plumbing lives in the unified sync-surface engine
//! ([`crate::storage::store_trait`]); this module keeps only the
//! identity-domain entry points.

use anyhow::Result;
use tokio::{join, try_join};

use crate::{
    models::app_state::AppState,
    storage::{
        account_table::AccountTable,
        automerge::merge_docs_async,
        background_errors::report_nonfatal,
        store_trait::{self, SyncSurface, merge_remote},
    },
    user::{
        User,
        sync_models::{SyncProfile, SyncSecurity},
    },
};

/// Best-effort: bring both identity docs in line with the user's current
/// synced fields. Failures are reported, not fatal — the pull path
/// re-reconciles current values before any remote merge.
pub async fn sync_docs_from_user(user: &User) {
    let account_id = user.account_id();
    if account_id.is_empty() {
        return;
    }
    let profile = SyncProfile::from(user);
    let security = SyncSecurity::from(user);
    let (profile_res, security_res) = join!(
        store_trait::reconcile_local_doc(account_id, &profile),
        store_trait::reconcile_local_doc(account_id, &security),
    );
    if let Err(e) = profile_res {
        report_nonfatal("user.identity_doc", "Failed to update local profile doc", e);
    }
    if let Err(e) = security_res {
        report_nonfatal(
            "user.identity_doc",
            "Failed to update local security doc",
            e,
        );
    }
}

/// Seed the local identity docs from raw pulled remote docs (pairing): the
/// new device imports the remote causal history so its future edits merge
/// as descendants instead of concurrent siblings. Merges into any existing
/// local docs rather than overwriting.
pub async fn seed_docs(
    account_id: &str,
    profile_doc: &[u8],
    security_doc: &[u8],
    app_state_doc: &[u8],
) -> Result<()> {
    try_join!(
        seed_doc::<SyncProfile>(account_id, profile_doc),
        seed_doc::<SyncSecurity>(account_id, security_doc),
        async {
            merge_remote::<AppState>(account_id.as_bytes(), app_state_doc)
                .await
                .map(|_| ())
        },
    )?;
    Ok(())
}

async fn seed_doc<T: SyncSurface>(account_id: &str, doc: &[u8]) -> Result<()> {
    let existing = store_trait::load_doc_bytes::<T>(account_id).await?;
    if let Some(merged) =
        merge_docs_async(existing.clone().map(Vec::from), Some(doc.to_vec())).await?
        && existing.as_deref() != Some(merged.as_slice())
    {
        store_trait::store_doc_bytes::<T>(account_id, &merged).await?;
    }
    Ok(())
}

/// Delete all identity docs for an account (account deletion cleanup).
pub async fn delete_docs(account_id: &str) -> Result<()> {
    AccountTable::new(store_trait::IDENTITY_DOC_TABLE)
        .delete_all_for_account(account_id)
        .await
}
