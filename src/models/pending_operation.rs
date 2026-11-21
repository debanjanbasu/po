//! Pending sync operation model for offline-first queue-based background sync.
//!
//! Operations are persisted to `TableDB` when offline and processed when network
//! becomes available.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, anyhow};
use thiserror::Error;
use veilid_core::bytes::Bytes;

use crate::{
    helpers::{
        async_utils::now_epoch_millis,
        capnp::{capnp_enum_convert, pack, unpack},
    },
    models::generated::pending_operation_capnp,
    storage::schema_version::{SchemaVersionError, unpack_versioned},
};

/// Stamped on serialization and validated on read.
///
/// The op's target is two typed fields: a peer record key and, for an invite
/// retry, the conversation's manifest key. They were once one
/// `"{peer}:{manifest}"` composite parsed with `split_once(':')`, which
/// truncated the peer key at the colon inside the Veilid record key itself,
/// so every invite was addressed to `"VLD0"`.
///
/// v1 means this two-field layout. It does not mean the old composite layout:
/// that one carried the same version number, so a row written by it would
/// decode as a valid v1 op with a composite target. Every device's queue was
/// wiped before this number was reused.
const PENDING_OP_SCHEMA_VERSION: u32 = 1;
const PENDING_OP_TYPE_NAME: &str = "PendingOperation";

/// A queue row that decoded structurally but that this build cannot use.
///
/// Two causes, one rule:
/// - a schema version older or newer than the running one, and
/// - an `OpType` ordinal this build does not know (new variants are added
///   without a schema bump, so the version alone cannot mark them).
///
/// Both mean another build understands the row and may still run it, so the
/// row must be skipped and kept — never deleted as corruption. Only rows that
/// fail to decode at all (or whose stored id disagrees with their key) are
/// corrupt.
#[derive(Debug, Error)]
#[error("queue row this build cannot use: {0}")]
pub struct UnsupportedOpRow(pub String);

/// Monotonic counter to guarantee unique IDs even when multiple operations
/// are created within the same millisecond.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Ensures newly generated operation IDs start at or above `next_id`.
pub fn seed_next_id(next_id: u64) {
    ID_COUNTER.fetch_max(next_id, Ordering::AcqRel);
}

/// Type of sync operation to execute against the DHT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpType {
    Profile,
    Security,
    ProfileAndSecurity,
    AppState,
    Totp,
    Chat,
    ChatContacts,
    CreateIdentity,
    ReencryptSubkeys,
    DictPublish,
    ConvFeedSync,
    ConvRecordCreate,
    ConvInviteSignal,
}

impl OpType {
    /// Stable small-integer discriminant, used for compact dedup-lock keys.
    #[must_use]
    pub const fn discriminant(self) -> u8 {
        self as u8
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Profile => "Profile",
            Self::Security => "Security",
            Self::ProfileAndSecurity => "Profile+Security",
            Self::AppState => "App State",
            Self::Totp => "TOTP",
            Self::Chat => "Chat",
            Self::ChatContacts => "Contacts",
            Self::CreateIdentity => "Identity",
            Self::ReencryptSubkeys => "Re-encrypt",
            Self::DictPublish => "Dictionary sync",
            Self::ConvFeedSync => "Feed sync",
            Self::ConvRecordCreate => "Conv bootstrap",
            Self::ConvInviteSignal => "Invite signal",
        }
    }

    /// Whether enqueueing this operation requires an established DHT identity.
    ///
    /// Every operation except [`OpType::CreateIdentity`] publishes on behalf
    /// of an existing identity, so the single queue entry point
    /// (`UserCtx::enqueue`) silently skips them (`Ok(())`) until onboarding
    /// has produced a `dht_key`. `CreateIdentity` is the bootstrap op that
    /// *creates* that identity and therefore enqueues unconditionally.
    #[must_use]
    pub const fn requires_identity(self) -> bool {
        !matches!(self, Self::CreateIdentity)
    }
}

capnp_enum_convert!(OpType => pending_operation_capnp::OpType {
    Profile,
    Security,
    ProfileAndSecurity,
    AppState,
    Totp,
    Chat,
    ChatContacts,
    CreateIdentity,
    ReencryptSubkeys,
    DictPublish,
    ConvFeedSync,
    ConvRecordCreate,
    ConvInviteSignal,
});

/// What distinguishes one queued op from another for the same account and
/// op type.
///
/// Most ops are addressed by a single target key. An invite retry is
/// addressed by a peer AND scoped to one conversation, so it needs both.
/// They are separate typed fields rather than one delimited string: a Veilid
/// record key already contains colons (`"<key>:<encryption_key>"`), so any
/// delimiter-joined composite can be split in the wrong place.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DedupTarget {
    /// A single target key (conversation record, manifest, …).
    Target(Arc<str>),
    /// An invite retry: the recipient peer and the conversation it opens.
    Invite { peer: Arc<str>, manifest: Arc<str> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOperation {
    pub id: u64,
    pub username: Arc<str>,
    pub op_type: OpType,
    pub payload: Bytes,
    pub created_at: i64,
    pub sequence_number: u64,
    pub attempts: u32,
    pub last_error: String,
    pub scheduled_at: u64,
    pub target_key: Option<Arc<str>>,
    /// Second half of a two-part target.
    ///
    /// An invite retry is addressed to a peer AND scoped to one
    /// conversation. They are separate fields because a Veilid record key
    /// contains colons of its own (`"<key>:<encryption_key>"`), so a single
    /// delimiter-joined string cannot be split safely.
    pub target_subkey: Option<Arc<str>>,
    pub identity_key: Arc<str>,
}

impl PendingOperation {
    pub fn new(
        username: &str,
        op_type: OpType,
        payload: impl Into<Bytes>,
        sequence_number: u64,
    ) -> Self {
        let millis = now_epoch_millis();
        // Use full 64-bit atomic counter for globally unique, monotonic IDs.
        let id = ID_COUNTER.fetch_add(1, Ordering::AcqRel);
        Self {
            id,
            username: Arc::from(username),
            op_type,
            payload: payload.into(),
            // Epoch millis fit in i64 until year 292277026; clamp unreachable
            // overflow defensively.
            created_at: i64::try_from(millis).unwrap_or(i64::MAX),
            sequence_number,
            attempts: 0,
            last_error: String::new(),
            scheduled_at: 0,
            target_key: None,
            target_subkey: None,
            identity_key: Arc::from(""),
        }
    }

    /// Encodes the message IDs covered by a conversation feed sync.
    ///
    /// IDs are sorted and newline-delimited for deterministic payloads.
    #[must_use]
    pub fn conv_feed_payload<'a>(message_ids: impl IntoIterator<Item = &'a str>) -> Bytes {
        let mut message_ids: Vec<_> = message_ids.into_iter().collect();
        message_ids.sort_unstable();
        message_ids.dedup();
        message_ids.join("\n").into_bytes().into()
    }

    /// Returns the message IDs covered by a conversation feed sync.
    ///
    /// An empty payload is a valid metadata-only sync. Nonempty payloads must
    /// be valid UTF-8 with no empty newline-delimited IDs.
    pub fn conv_feed_message_ids(&self) -> Result<impl Iterator<Item = &str> + '_> {
        let payload = str::from_utf8(&self.payload)
            .map_err(|e| anyhow!("Invalid ConvFeedSync message-ID payload: {e}"))?;
        if !self.payload.is_empty() && payload.split('\n').any(str::is_empty) {
            return Err(anyhow!("Invalid ConvFeedSync message-ID payload: empty ID"));
        }
        Ok(payload.split('\n').filter(|id| !id.is_empty()))
    }

    /// The peer an invite retry is addressed to, read from the op's target.
    ///
    /// The target is the peer's record key, verbatim. It used to be a
    /// `"{peer}:{manifest}"` composite that callers recovered with
    /// `split_once(':')` — but a Veilid record key already contains colons
    /// (`"<key>:<encryption_key>"`), so that split truncated the peer key to
    /// `"VLD0"` and the invite was addressed to nobody.
    #[must_use]
    pub fn invite_peer_key(&self) -> Option<&str> {
        self.target_key.as_deref()
    }

    /// The dedup discriminator for this op.
    ///
    /// For most ops this is the target key alone. An invite retry also needs
    /// the manifest key, because two conversations with the same peer are two
    /// distinct deliveries — without it, enqueuing the second would squash
    /// the first pending invite.
    ///
    /// Both halves are typed fields, never a delimited string, so a record
    /// key that contains colons cannot split the discriminator.
    #[must_use]
    pub fn dedup_target(&self) -> Option<DedupTarget> {
        let target = self.target_key.clone()?;
        if self.op_type != OpType::ConvInviteSignal {
            return Some(DedupTarget::Target(target));
        }
        Some(DedupTarget::Invite {
            peer: target,
            manifest: self.target_subkey.clone().unwrap_or_else(|| Arc::from("")),
        })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        pack::<pending_operation_capnp::PendingOperation>(|mut root| {
            root.schema_version().set(PENDING_OP_SCHEMA_VERSION);
            root.id().set(self.id);
            root.username().set_str(&self.username);
            root.op_type().set(self.op_type.into());
            root.payload().set_slice(&self.payload);
            root.created_at().set(self.created_at);
            root.sequence_number().set(self.sequence_number);
            root.attempts().set(self.attempts);
            root.last_error().set_str(&self.last_error);
            root.scheduled_at().set(self.scheduled_at);
            if let Some(tk) = &self.target_key {
                root.target_key().set_str(tk);
            }
            if let Some(tsk) = &self.target_subkey {
                root.target_subkey().set_str(tsk);
            }
            root.identity_key().set_str(&self.identity_key);
        })
    }

    fn unpack_with_version(bytes: &[u8]) -> Result<(Self, u32)> {
        unpack::<pending_operation_capnp::PendingOperation, _>(bytes, |root| {
            let version = root.schema_version();
            // An unknown ordinal is a variant a newer build added: the row is
            // structurally sound, this build just cannot execute it.
            let op_type = OpType::from(
                root.op_type()
                    .map_err(|e| UnsupportedOpRow(format!("unknown op_type ordinal: {e}")))?,
            );
            let opt_arc = |v: &str| (!v.is_empty()).then(|| Arc::from(v));
            Ok((
                Self {
                    id: root.id(),
                    username: root.username().get().as_str()?.into(),
                    op_type,
                    payload: Bytes::copy_from_slice(root.payload().get().as_slice()),
                    created_at: root.created_at(),
                    sequence_number: root.sequence_number(),
                    attempts: root.attempts(),
                    last_error: root.last_error().get().as_str()?.to_string(),
                    scheduled_at: root.scheduled_at(),
                    target_key: opt_arc(root.target_key().get().as_str()?),
                    target_subkey: opt_arc(root.target_subkey().get().as_str()?),
                    identity_key: Arc::from(root.identity_key().get().as_str()?),
                },
                version,
            ))
        })
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        unpack_versioned(
            PENDING_OP_TYPE_NAME,
            PENDING_OP_SCHEMA_VERSION,
            bytes,
            Self::unpack_with_version,
        )
        .map_err(|e| {
            // A version mismatch is also "decoded but unusable": the row was
            // written by a build this one replaced (or follows). Keep the
            // distinction in the message for the queue's log, but classify it
            // with the same typed error so no caller can delete it.
            if e.downcast_ref::<SchemaVersionError>().is_some() {
                UnsupportedOpRow(e.to_string()).into()
            } else {
                e
            }
        })
        .map(|(op, _version)| op)
    }
}
