//! Identity sync context — DHT operations for identity DFLT records.

use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{Context, Error, Result, anyhow, ensure};
use dioxus::logger::tracing::warn;
use veilid_core::{KeyPair, RecordKey, SharedSecret};
use zeroize::Zeroizing;

use crate::{
    helpers::store_lock::lock_store,
    models::pending_operation::OpType,
    session::epoch::SessionGuard,
    session::phase::mutate_authenticated_user,
    storage::{
        automerge::AutomergeSyncError,
        background_errors::report_nonfatal,
        store_trait::{self, CrdtModel, LiveDoc, SyncSurface},
    },
    user::{
        self, CURRENT_CRYPTO_VERSION, User,
        auth::decrypt_keypair,
        ctx::UserCtx,
        store::{load_matching_user, load_password_hash_opt, store_user},
        sync_models::FeatureId,
    },
    veilid::core::parse_record_key,
    veilid::store,
};

/// Bundles identity-specific DHT state: record key, writer keypair,
/// password-derived secret, and the loaded user. Used for identity DFLT
/// record operations (profile, security, pointers, child records).
#[derive(Clone)]
pub struct IdentityContext {
    pub user: Arc<User>,
    pub key: RecordKey,
    pub keypair: KeyPair,
    pub secret: SharedSecret,
}

impl IdentityContext {
    /// Resolve context from a stored user record.
    ///
    /// Tries verified live session crypto first; falls back to deriving the DHT
    /// secret from the persisted password hash and decrypting the keypair from
    /// the stored encrypted blob when no live session crypto is available. The
    /// keypair fallback requires "remember me" to be active on this device
    /// (so the password hash is persisted locally).
    pub async fn for_user(username: &str) -> Result<Self> {
        let _user_lock = lock_store(username.as_bytes()).await;
        let user = user::load_user(username)
            .await?
            .ok_or_else(|| anyhow!("User not found: {username}"))?;
        Self::from_stored_user(user).await
    }

    pub(super) async fn from_stored_user(user: User) -> Result<Self> {
        ensure!(
            user.crypto_version() == CURRENT_CRYPTO_VERSION,
            "Unsupported account crypto version for identity access"
        );
        if user.dht_key().is_empty() {
            return Err(anyhow!("User has no DHT key"));
        }
        let key = parse_record_key(user.dht_key()).map_err(|e| anyhow!("Invalid DHT key: {e}"))?;
        let (keypair, secret) = Self::crypto_for_user(&user).await?;
        let user = Arc::new(user);

        Ok(Self {
            user,
            key,
            keypair,
            secret,
        })
    }

    pub(super) async fn crypto_for_user(user: &User) -> Result<(KeyPair, SharedSecret)> {
        if let Some(session) = user::get_verified_session_crypto(user) {
            return Ok((session.keypair.clone(), session.dht_secret.clone()));
        }
        let password_hash = Zeroizing::new(
            load_password_hash_opt(user.username())
                .await?
                .context("No stored credential or matching session crypto for this account")?,
        );
        decrypt_keypair(
            &password_hash,
            user.account_id(),
            user.keypair_encrypted(),
            user.nonce(),
        )
        .await
        .context("Failed to recover account crypto from the remembered credential")
    }

    /// Account ID as bytes — the standard local `TableDB` key for per-user storage.
    pub fn key_bytes(&self) -> &[u8] {
        self.user.account_id().as_bytes()
    }

    // ── Unified sync-surface methods ──────────────────────────────────

    /// Project the latest account snapshot before merging remote history;
    /// fold the published history back into the latest local snapshot.
    pub(crate) async fn sync_subkey<T: SyncSurface + PartialEq + for<'a> From<&'a User>>(
        &self,
        apply: fn(&T, &mut User),
    ) -> Result<()> {
        let account_id = self.user.account_id();
        let (value, doc) = {
            let _user_lock = lock_store(self.user.username().as_bytes()).await;
            let current = load_matching_user(self.user.username(), account_id)
                .await?
                .context("Identity publish account was deleted or replaced")?;
            let _account_lock = lock_store(self.key_bytes()).await;
            let value = T::from(&current);
            let doc = store_trait::reconcile_local_doc(account_id, &value).await?;
            (value, doc)
        };
        let written = self.publish_identity_subkey(&value, &doc).await?;
        let _user_lock = lock_store(self.user.username().as_bytes()).await;
        let Some(mut current) = load_matching_user(self.user.username(), account_id).await? else {
            return Ok(());
        };
        let _account_lock = lock_store(self.key_bytes()).await;
        let current_value = T::from(&current);
        if current_value == value && written == doc {
            return Ok(());
        }
        if current_value != value {
            store_trait::reconcile_local_doc(account_id, &current_value).await?;
        }
        let (merged, remote_stale) =
            store_trait::merge_remote_doc::<T>(account_id, &written).await?;
        let onboarded = !current.needs_onboarding();
        apply(&merged, &mut current);
        let regresses_onboarding = onboarded && current.needs_onboarding();
        if regresses_onboarding {
            apply(&current_value, &mut current);
            store_trait::reconcile_local_doc(account_id, &current_value).await?;
        }
        store_user(&current).await?;
        mutate_authenticated_user(|active| {
            if active.account_id() == account_id {
                apply(&T::from(&current), active);
            }
        });
        if remote_stale || regresses_onboarding {
            UserCtx::from_user(&current)
                .enqueue_touch(OpType::ProfileAndSecurity)
                .await?;
        }
        Ok(())
    }

    /// Publish a prepared local document. The caller merges the returned
    /// history under its account lock; it must not blindly overwrite a doc
    /// that could have changed during the network request.
    pub(crate) async fn publish_identity_subkey<T: SyncSurface>(
        &self,
        value: &T,
        local: &[u8],
    ) -> Result<Vec<u8>> {
        store::reconcile_write(
            &self.key,
            Some(&self.keypair),
            T::SUBKEY,
            &self.secret,
            true,
            Some(local),
            value,
        )
        .await
        .map_err(Error::from)
    }

    /// The publish sequence every live-doc-backed surface shares: reconcile a
    /// local doc against its DHT subkey, then fold the written history back
    /// into the local doc under the user lock — and only while this account
    /// still exists, since the network write can outlive it.
    ///
    /// The outer `Err` is a local load/persist failure, which callers surface
    /// bare; the inner result is the DHT write's own outcome, kept typed
    /// because callers inspect it (schema corruption) and wrap it themselves.
    /// The inner `None` means the account was deleted or replaced mid-flight,
    /// so the written history was deliberately *not* folded back in; `Some` is
    /// the folded-in local state — the value a load at fold time reports, which
    /// saves a caller that hydrates from it a second read of the row.
    pub(crate) async fn publish_live_doc<'a, T: CrdtModel>(
        &self,
        target_key: &RecordKey,
        writer: Option<&KeyPair>,
        current: Option<&'a LiveDoc<T>>,
    ) -> Result<Result<Option<Cow<'a, T>>, store::ReconcileWriteError>, Error> {
        // No local doc yet: publish the default value with no base history.
        let (value, local_doc) = current.map_or_else(
            || (Cow::Owned(T::default()), None),
            |doc| (Cow::Borrowed(&doc.value), Some(doc.doc.as_slice())),
        );

        // Forced base read — same reasoning as `publish_identity_subkey`.
        let result = store::reconcile_write(
            target_key,
            writer,
            T::SUBKEY,
            &self.secret,
            true,
            local_doc,
            &*value,
        )
        .await;

        let (Ok(written), Some(state)) = (&result, current) else {
            // No local row yet: seed it from the network's own doc so the
            // local doc descends from the network's automerge history. A
            // doc built from the snapshot alone would be an independent
            // root, and automerge replaces a whole map object when two
            // roots merge — the next publish would then drop the local
            // side's keys.
            let Ok(written) = &result else {
                return Ok(result.map(|_| None));
            };
            let _user_lock = lock_store(self.user.username().as_bytes()).await;
            if load_matching_user(self.user.username(), self.user.account_id())
                .await?
                .is_none()
            {
                return Ok(Ok(None));
            }
            let seeded = store_trait::seed_local_doc::<T>(self.key_bytes(), written).await?;
            return Ok(Ok(Some(Cow::Owned(seeded))));
        };
        let _user_lock = lock_store(self.user.username().as_bytes()).await;
        if load_matching_user(self.user.username(), self.user.account_id())
            .await?
            .is_none()
        {
            return Ok(Ok(None));
        }
        let merged =
            store_trait::persist_written_doc::<T>(self.key_bytes(), written, state).await?;
        Ok(Ok(Some(merged)))
    }

    /// Resolve child record key and sync the model to its child DHT record
    /// with the same symmetric CRDT semantics as identity subkeys: local
    /// doc history is merged with the network base before reconciling, and
    /// the written doc is persisted back as the new local doc.
    pub(crate) async fn sync_child_crdt<T: CrdtModel>(&self) -> Result<()> {
        let (child_key, child_writer) = super::record::resolve_child_key_for_id(
            self.key.clone(),
            &self.secret,
            &self.keypair,
            T::ID,
            T::CHILD_SUBKEYS,
        )
        .await
        .context("resolve_child_key_for_id failed")?;

        let current = store_trait::locked_live_doc::<T>(self.key_bytes()).await?;
        let result = self
            .publish_live_doc::<T>(&child_key, Some(&child_writer), current.as_ref())
            .await?;

        if result.as_ref().err().is_some_and(is_write_schema_error) {
            apply_schema_fallback::<T>(
                self.user.account_id(),
                Some(self.user.username()),
                None,
                true,
            )
            .await;
        }

        result
            .map(|_written| ())
            .map_err(Error::from)
            .context("sync_to_dht failed")
    }
}

/// True when a DHT-write error reports a schema-validation failure — a
/// remote document this build cannot hydrate.
fn is_write_schema_error(e: &store::ReconcileWriteError) -> bool {
    matches!(
        e,
        store::ReconcileWriteError::Automerge(ae)
            if matches!(ae.as_ref(), AutomergeSyncError::SchemaValidation(_))
    )
}

/// True when an anyhow error chain from the pull/merge path reports a
/// schema-validation failure ([`AutomergeSyncError`] surfaces directly).
pub fn is_merge_schema_error(err: &Error) -> bool {
    err.downcast_ref::<AutomergeSyncError>()
        .is_some_and(|ae| matches!(ae, AutomergeSyncError::SchemaValidation(_)))
}

/// Per-surface schema-corruption fallback policy, applied consistently on
/// publish, watcher pulls, and pairing resync pulls: when a child record's
/// network copy fails schema validation, clear the cached feature pointer
/// (in-memory session user and persisted user) so the next resolve recreates
/// the record fresh. Identity subkeys have no pointer to clear — a no-op.
///
/// Scoped by account: the session-side clear only touches a user whose
/// account matches `account_id`, so lanes syncing one account never mutate
/// another's session state. The durable half is username-addressed (the
/// `po_users` blob); lanes that have not resolved a username for the account
/// yet skip it — with no persisted user there is no durable pointer cache to
/// clear either.
pub async fn apply_schema_fallback<T: CrdtModel>(
    account_id: &str,
    username: Option<&str>,
    guard: Option<SessionGuard>,
    schema_validation: bool,
) {
    if guard.is_some_and(|guard| !guard.is_current()) || !schema_validation {
        return;
    }
    warn!(
        "Stale child record for {}, clearing pointer and persisting",
        T::DISPLAY_NAME
    );
    let fid = FeatureId::new(T::ID);
    mutate_authenticated_user(|user| {
        if guard.is_none_or(SessionGuard::is_current)
            && user.account_id() == account_id
            && let Some(ptrs) = &mut user.runtime.sync_pointers
        {
            ptrs.pointers.remove(&fid);
        }
    });
    let Some(username) = username else {
        return;
    };
    let result: Result<()> = try {
        let _user_lock = lock_store(username.as_bytes()).await;
        // Re-check before the row load: a session that went stale while the
        // error funnelled here must not pay a serialized load under this lock.
        if guard.is_none_or(SessionGuard::is_current)
            && let Some(mut persisted_user) = load_matching_user(username, account_id).await?
            && let Some(ptrs) = &mut persisted_user.runtime.sync_pointers
        {
            ptrs.pointers.remove(&fid);
            store_user(&persisted_user).await?;
        }
    };
    if let Err(error) = result {
        report_nonfatal(
            "identity.context",
            "Failed to clear account sync pointer after schema validation fallback",
            error,
        );
    }
}
