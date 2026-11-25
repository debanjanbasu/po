//! Lightweight user context extracted from the `SESSION` global signal.
//!
//! `UserCtx` captures the authenticated user's identity fields in a single
//! read, avoiding repeated signal access and the manual extraction dance
//! that previously spread across 20+ files.
//!
//! Construct via [`UserCtx::try_current()`] (from the Dioxus runtime) or
//! [`UserCtx::from_user()`] (from a `&User` reference outside the runtime).

use std::sync::Arc;

use anyhow::Result;
use dioxus::prelude::ReadableExt;
use tokio::sync::OwnedMutexGuard;
use veilid_core::{KeyPair, bytes::Bytes};

use crate::{
    helpers::store_lock::lock_store,
    models::pending_operation::{OpType, PendingOperation},
    session::{epoch::SessionGuard, phase::SESSION},
    sync::{pending_ops, queue::is_account_deleted},
    user::store::load_matching_user,
};

/// Snapshot of the authenticated user's identity, extracted once from `SESSION`.
///
/// Cheap to clone (all fields are `Arc<str>` or small enum).
/// Use [`UserCtx::try_current()`] to extract from the `SESSION` signal.
#[derive(Clone, PartialEq, Eq)]
pub struct UserCtx {
    pub username: Arc<str>,
    pub account_id: Arc<str>,
    pub dht_key: Arc<str>,
    pub keypair: Option<KeyPair>,
}

impl UserCtx {
    /// Reads the `SESSION` signal once and returns `Some(UserCtx)` if logged in.
    #[must_use]
    pub fn try_current() -> Option<Self> {
        SESSION.with(|s| {
            s.authenticated_user().map(|u| Self {
                username: u.username_arc(),
                account_id: u.account_id_arc(),
                dht_key: u.dht_key_arc(),
                keypair: u.runtime.keypair.clone(),
            })
        })
    }

    /// Capture the current identity and its epoch before asynchronous work.
    #[must_use]
    pub(crate) fn try_current_with_guard() -> Option<(Self, SessionGuard)> {
        let guard = SessionGuard::capture();
        let ctx = Self::try_current()?;
        guard.is_current().then_some((ctx, guard))
    }

    /// Hold the user-row lock while verifying this context still owns its account.
    /// Keep the returned guard alive through the corresponding durable commit.
    pub(crate) async fn lock_for_account_commit(
        &self,
        guard: SessionGuard,
    ) -> Result<Option<OwnedMutexGuard<()>>> {
        if !guard.is_current() {
            return Ok(None);
        }
        let Some(user_lock) = self.lock_for_persisted_account().await? else {
            return Ok(None);
        };
        if !guard.is_current() {
            return Ok(None);
        }
        Ok(Some(user_lock))
    }

    /// Hold the username row lock and confirm this account still exists.
    /// Unlike `lock_for_account_commit`, this also supports durable work that
    /// intentionally continues for an inactive account. Sync-queue tombstoned
    /// accounts resolve to `None` like missing rows. The tombstone check runs
    /// both before (cheap early-out) and after acquiring the lock (races with
    /// concurrent deletion are excluded by re-checking under the lock).
    pub(crate) async fn lock_for_persisted_account(&self) -> Result<Option<OwnedMutexGuard<()>>> {
        if is_account_deleted(&self.account_id) {
            return Ok(None);
        }
        let user_lock = lock_store(self.username.as_bytes()).await;
        if is_account_deleted(&self.account_id)
            || load_matching_user(&self.username, &self.account_id)
                .await?
                .is_none()
        {
            return Ok(None);
        }
        Ok(Some(user_lock))
    }

    /// Minimal context for durable queue work: no keypair or DHT key needed.
    #[must_use]
    pub(crate) fn for_queue_op(username: Arc<str>, account_id: Arc<str>) -> Self {
        Self {
            username,
            account_id,
            dht_key: Arc::from(""),
            keypair: None,
        }
    }
    /// Construct from a `User` reference — for use outside the Dioxus runtime
    /// (e.g., onboarding, password change).
    #[must_use]
    pub fn from_user(user: &super::User) -> Self {
        Self {
            username: user.username_arc(),
            account_id: user.account_id_arc(),
            dht_key: user.dht_key_arc(),
            keypair: user.runtime.keypair.clone(),
        }
    }

    /// Local `TableDB` key — `account_id` as bytes.
    #[must_use]
    pub fn storage_key(&self) -> &[u8] {
        self.account_id.as_bytes()
    }

    /// Creates a `PendingOperation` with `identity_key` pre-set.
    fn new_op(&self, op_type: OpType, payload: impl Into<Bytes>) -> PendingOperation {
        let seq = pending_ops::next_sequence_number();
        let mut op = PendingOperation::new(&self.username, op_type, payload, seq);
        op.identity_key = Arc::clone(&self.account_id);
        op
    }

    /// Creates and enqueues a `PendingOperation` — the single queue entry point.
    ///
    /// Gating is a property of the operation ([`OpType::requires_identity`]),
    /// not a caller choice: identity-gated ops are silently skipped
    /// (`Ok(())`) for pre-onboarding users; [`OpType::CreateIdentity`] always
    /// enqueues. Each enqueue gets a fresh id + sequence number; the queue's
    /// squash/dedup pointer moves to the new row.
    pub async fn enqueue(&self, op_type: OpType, payload: impl Into<Bytes>) -> Result<()> {
        self.enqueue_op(self.new_op(op_type, payload)).await
    }

    /// Enqueue a payload-less republish ("touch") op.
    ///
    /// Handlers for these op types rebuild the synced data from local state,
    /// so nothing is carried in the payload. Same gating as [`Self::enqueue`].
    pub async fn enqueue_touch(&self, op_type: OpType) -> Result<()> {
        self.enqueue(op_type, Bytes::new()).await
    }

    /// Enqueue a `PendingOperation` carrying a payload aimed at a target key
    /// (e.g. a conversation record). Same gating as [`Self::enqueue`].
    pub async fn enqueue_for_target(
        &self,
        op_type: OpType,
        payload: impl Into<Bytes>,
        target_key: &str,
    ) -> Result<()> {
        let mut op = self.new_op(op_type, payload);
        op.target_key = Some(Arc::from(target_key));
        self.enqueue_op(op).await
    }

    /// Enqueue a `PendingOperation` aimed at two keys: a target and a scope.
    ///
    /// An invite retry is addressed to a peer AND scoped to one
    /// conversation. The two are separate fields rather than one joined
    /// string, because a Veilid record key contains colons of its own and
    /// cannot be split back out of a composite safely.
    pub async fn enqueue_for_scoped_target(
        &self,
        op_type: OpType,
        payload: impl Into<Bytes>,
        target_key: &str,
        target_subkey: &str,
    ) -> Result<()> {
        let mut op = self.new_op(op_type, payload);
        op.target_key = Some(Arc::from(target_key));
        op.target_subkey = Some(Arc::from(target_subkey));
        self.enqueue_op(op).await
    }

    /// Applies the per-`OpType` identity gate once, then persists to the queue.
    async fn enqueue_op(&self, op: PendingOperation) -> Result<()> {
        if op.op_type.requires_identity() && !self.has_identity() {
            return Ok(());
        }
        pending_ops::enqueue(op).await?;
        Ok(())
    }

    /// Whether this user has a DHT identity (completed onboarding).
    #[must_use]
    pub fn has_identity(&self) -> bool {
        !self.dht_key.is_empty()
    }
}
