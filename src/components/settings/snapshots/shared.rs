//! Shared backup types and data-packing helpers for the snapshot screens.

use crate::{
    chat::{contact::ContactList, conversation::ConversationIndex},
    models::totp::TotpTokenList,
    session::phase::SESSION,
    storage::{
        backup::{self, BackupSection},
        snapshots::{self, SnapshotEntry},
        store_trait::{CrdtModel, load_local},
    },
    user::ctx::UserCtx,
};
use anyhow::{Result, anyhow, ensure};
use dioxus::prelude::{ReadableExt, Signal};

/// The three user-data surfaces surfaced by the export and snapshot UIs.
///
/// Single source of truth replacing the former parallel `SnapshotTab` /
/// `ExportMode` enums; dispatch goes through each surface's model type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SyncSurface {
    Totp,
    Contacts,
    Conversations,
}

impl SyncSurface {
    /// All surfaces in UI display order.
    pub(super) const ALL: [Self; 3] = [Self::Totp, Self::Contacts, Self::Conversations];

    /// Tab / button label.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Totp => "TOTP",
            Self::Contacts => "Contacts",
            Self::Conversations => "Conversations",
        }
    }

    /// Filename for a single-surface backup download.
    pub(super) const fn filename(self) -> &'static str {
        match self {
            Self::Totp => "po-totp.pobk",
            Self::Contacts => "po-contacts.pobk",
            Self::Conversations => "po-chats.pobk",
        }
    }
}

/// List archived snapshots for a surface.
pub(super) async fn list_surface_snapshots(
    surface: SyncSurface,
    key: &[u8],
) -> Result<Vec<SnapshotEntry>> {
    match surface {
        SyncSurface::Totp => snapshots::list_snapshots::<TotpTokenList>(key).await,
        SyncSurface::Contacts => snapshots::list_snapshots::<ContactList>(key).await,
        SyncSurface::Conversations => snapshots::list_snapshots::<ConversationIndex>(key).await,
    }
}

/// Restore a surface's model to the snapshot captured at `timestamp`.
///
/// Returns `false` when the device was already at that snapshot.
pub(super) async fn restore_surface_snapshot(
    surface: SyncSurface,
    key: &[u8],
    timestamp: u64,
) -> Result<bool> {
    match surface {
        SyncSurface::Totp => snapshots::restore_to_snapshot::<TotpTokenList>(key, timestamp).await,
        SyncSurface::Contacts => {
            snapshots::restore_to_snapshot::<ContactList>(key, timestamp).await
        }
        SyncSurface::Conversations => {
            snapshots::restore_to_snapshot::<ConversationIndex>(key, timestamp).await
        }
    }
}

/// Load a surface's model from local storage and pack it into a backup
/// section (single-surface exports).
pub(super) async fn pack_surface(
    surface: SyncSurface,
    key: &[u8],
) -> Result<BackupSection, String> {
    match surface {
        SyncSurface::Totp => load_and_pack::<TotpTokenList>(key).await,
        SyncSurface::Contacts => load_and_pack::<ContactList>(key).await,
        SyncSurface::Conversations => load_and_pack::<ConversationIndex>(key).await,
    }
}

/// Load a surface's model and create a manual snapshot of it.
pub(super) async fn create_surface_snapshot(
    surface: SyncSurface,
    key: &[u8],
) -> Result<(), String> {
    match surface {
        SyncSurface::Totp => create_model_snapshot::<TotpTokenList>(key).await,
        SyncSurface::Contacts => create_model_snapshot::<ContactList>(key).await,
        SyncSurface::Conversations => create_model_snapshot::<ConversationIndex>(key).await,
    }
}

/// Load a model and create a manual snapshot for it.
async fn create_model_snapshot<T: CrdtModel>(key: &[u8]) -> Result<(), String> {
    let v = load_model::<T>(key)
        .await
        .map_err(|detail| format!("Failed to load {}: {detail}", T::DISPLAY_NAME))?;
    snapshots::create_manual_snapshot::<T>(key, &v)
        .await
        .map_err(|e| format!("Failed to create snapshot: {e}"))?;
    Ok(())
}

/// Decrypted backup sections awaiting merge/replace decision.
pub(super) type BackupSections = Vec<(String, Vec<u8>)>;

#[derive(Clone, Copy, PartialEq)]
pub(super) struct ImportState {
    pub(super) file_bytes: Signal<Option<Vec<u8>>>,
    pub(super) password: Signal<String>,
    pub(super) error: Signal<Option<String>>,
    pub(super) importing: Signal<bool>,
    pub(super) sections: Signal<Option<BackupSections>>,
    pub(super) success: Signal<bool>,
}

/// Load a surface's model from local storage, reporting the failure detail
/// bare — each caller adds the context its own surface's UI copy needs.
async fn load_model<T: CrdtModel>(key: &[u8]) -> Result<T, String> {
    load_local::<T>(key).await.map_err(|e| e.to_string())
}

/// Load a model from local storage and pack it into a backup section.
pub(super) async fn load_and_pack<T: CrdtModel>(key: &[u8]) -> Result<BackupSection, String> {
    let data = load_model::<T>(key)
        .await?
        .pack()
        .map_err(|e| e.to_string())?;
    Ok(BackupSection {
        model_id: T::ID,
        data,
    })
}

/// Build the identity backup section from the current session's User record.
pub(super) fn build_identity_section(ctx: &UserCtx) -> Result<Vec<u8>> {
    let user = SESSION.read().authenticated_user().cloned();
    let user = user.ok_or_else(|| anyhow!("No user in session"))?;

    ensure!(
        !user.keypair_encrypted().is_empty(),
        "Encrypted keypair not available — user may not be fully authenticated"
    );

    let identity = backup::IdentityBackup {
        username: ctx.username.to_string(),
        dht_key: ctx.dht_key.to_string(),
        account_id: ctx.account_id.to_string(),
        keypair_nonce: user
            .nonce()
            .try_into()
            .map_err(|_| anyhow!("Invalid keypair nonce length"))?,
        keypair_ciphertext: user.keypair_encrypted().to_vec(),
    };
    Ok(identity.encode())
}
