//! Wire model and hand-rolled binary codec for `PeerSignal`/`SenderProof`:
//! Veilid key, public-key, and signature framing plus encode/decode for every
//! signal payload.

use std::{str, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use veilid_core::{
    BareOpaqueRecordKey, BarePublicKey, BareRecordKey, BareSignature, CryptoKind, KeyPair,
    PublicKey, RecordKey, Signature,
};

use crate::{
    config,
    helpers::{
        encryption::crypto_system,
        framing::{push_u16_len_prefixed, read_u16_len_prefixed},
    },
};

/// Typed signal body carried by a [`PeerSignal`], before the sender-proof
/// tail is appended on the wire.
///
/// Conversation-scoped signals are sent with a sender proof because Veilid
/// private routes do not expose a trusted sender identity when sender privacy
/// is enabled. The proof lives on the envelope, not per variant, so a new
/// variant can never forget to carry it.
#[derive(Debug, Clone)]
pub enum SignalPayload {
    /// Remote user started typing in conversation.
    Typing {
        conv_key: RecordKey,
        sender_identity: RecordKey,
    },
    /// Remote user stopped typing.
    StopTyping {
        conv_key: RecordKey,
        sender_identity: RecordKey,
    },
    /// New data written to conversation DHT record — pull immediately.
    NewData {
        conv_key: RecordKey,
        sender_identity: RecordKey,
        member_subkey: u32,
    },
    /// Invitation to join a new conversation.
    ConversationInvite {
        manifest_key: RecordKey,
        inviter_key: RecordKey,
    },
    /// Invite recipient acknowledges durable receipt of an invite.
    ConversationInviteAck {
        manifest_key: RecordKey,
        inviter_key: RecordKey,
        invitee_key: RecordKey,
    },
    /// Same-account device nudge: the sender published identity-scoped
    /// data to the DHT, so sibling devices should pull the affected
    /// scopes immediately. `scope` carries [`crate::config::nudge`] bits;
    /// `device_id` identifies the sending device so receivers can ignore
    /// their own nudges.
    SyncNudge {
        identity_key: RecordKey,
        scope: u8,
        device_id: Arc<str>,
    },
}

/// Wire envelope: a typed [`SignalPayload`] plus an optional [`SenderProof`].
#[derive(Debug, Clone)]
pub struct PeerSignal {
    pub payload: SignalPayload,
    pub(in crate::chat) proof: Option<SenderProof>,
}

/// Sender proof signed by the sender's identity-record writer keypair.
#[derive(Debug, Clone)]
pub struct SenderProof {
    pub(crate) sender_public_key: PublicKey,
    pub(crate) signature: Signature,
}

impl SenderProof {
    pub(crate) async fn sign(message: &[u8], keypair: &KeyPair) -> Result<Self> {
        crypto_system!(crypto);
        if keypair.kind() != crypto.kind() {
            bail!(
                "unsupported crypto kind for sender proof: {}",
                keypair.kind()
            );
        }
        let sender_public_key = keypair.key();
        let signature = crypto
            .sign(&sender_public_key, &keypair.secret(), message)
            .context("failed to sign sender proof")?;
        Ok(Self {
            sender_public_key,
            signature,
        })
    }

    pub(crate) async fn verify(&self, message: &[u8]) -> Result<bool> {
        crypto_system!(crypto);
        if self.sender_public_key.kind() != crypto.kind() {
            bail!(
                "unsupported crypto kind for sender proof verification: {}",
                self.sender_public_key.kind(),
            );
        }
        crypto
            .verify(&self.sender_public_key, message, &self.signature)
            .context("failed to verify sender proof")
    }
}

/// Wire protocol tag identifying the signal type.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SignalTag {
    Typing = 0x01,
    StopTyping = 0x02,
    /// Reserved for the removed `ReadUpTo` signal; never reuse or renumber later tags.
    ReservedReadUpTo = 0x03,
    NewData = 0x04,
    ConversationInvite = 0x05,
    ConversationInviteAck = 0x06,
    SyncNudge = 0x07,
}

impl TryFrom<u8> for SignalTag {
    type Error = u8;
    fn try_from(value: u8) -> Result<Self, u8> {
        match value {
            0x01 => Ok(Self::Typing),
            0x02 => Ok(Self::StopTyping),
            0x03 => Ok(Self::ReservedReadUpTo),
            0x04 => Ok(Self::NewData),
            0x05 => Ok(Self::ConversationInvite),
            0x06 => Ok(Self::ConversationInviteAck),
            0x07 => Ok(Self::SyncNudge),
            other => Err(other),
        }
    }
}

/// Writes a `RecordKey` as 36 fixed bytes: `[4B kind][32B key]`.
fn key_to_wire(buf: &mut Vec<u8>, key: &RecordKey) {
    buf.extend_from_slice(key.kind().bytes());
    buf.extend_from_slice(key.ref_value().ref_key().as_ref());
}

fn key_from_wire(bytes: &[u8]) -> Result<(RecordKey, usize)> {
    if bytes.len() < config::crypto::RECORD_KEY_WIRE_LEN {
        bail!(
            "key wire data too short: {} < {}",
            bytes.len(),
            config::crypto::RECORD_KEY_WIRE_LEN
        );
    }
    Ok((
        RecordKey::new(
            CryptoKind::from(<[u8; 4]>::try_from(&bytes[..4])?),
            BareRecordKey::new(
                BareOpaqueRecordKey::from(&bytes[4..config::crypto::RECORD_KEY_WIRE_LEN]),
                None,
            ),
        ),
        config::crypto::RECORD_KEY_WIRE_LEN,
    ))
}

fn public_key_to_wire(buf: &mut Vec<u8>, key: &PublicKey) {
    buf.extend_from_slice(key.kind().bytes());
    push_u16_len_prefixed(buf, key.ref_value().as_ref());
}

fn public_key_from_wire(bytes: &[u8]) -> Result<(PublicKey, usize)> {
    let kind_bytes = bytes
        .get(..config::crypto::CRYPTO_KIND_LEN)
        .context("public key kind bytes")?;
    let kind = CryptoKind::from(<[u8; 4]>::try_from(kind_bytes)?);
    let (value, end) = read_u16_len_prefixed(bytes, config::crypto::CRYPTO_KIND_LEN, "public key")?;
    Ok((PublicKey::new(kind, BarePublicKey::new(value)), end))
}

fn signature_to_wire(buf: &mut Vec<u8>, signature: &Signature) {
    push_u16_len_prefixed(buf, signature.ref_value().as_ref());
}

fn signature_from_wire(bytes: &[u8], kind: CryptoKind) -> Result<(Signature, usize)> {
    let (value, end) = read_u16_len_prefixed(bytes, 0, "signature")?;
    Ok((Signature::new(kind, BareSignature::new(value)), end))
}

fn sender_proof_from_wire(bytes: &[u8]) -> Result<SenderProof> {
    if bytes.is_empty() {
        bail!("missing sender proof bytes");
    }
    let (sender_public_key, used) = public_key_from_wire(bytes)?;
    let (signature, sig_used) = signature_from_wire(&bytes[used..], sender_public_key.kind())?;
    let total_used = used + sig_used;
    if bytes.len() != total_used {
        bail!(
            "unexpected trailing sender proof bytes: {}",
            bytes.len().saturating_sub(total_used)
        );
    }
    Ok(SenderProof {
        sender_public_key,
        signature,
    })
}

impl PeerSignal {
    /// Wraps a payload into a signal without a sender proof.
    #[must_use]
    pub(crate) const fn unsigned(payload: SignalPayload) -> Self {
        Self {
            payload,
            proof: None,
        }
    }

    pub(crate) const fn kind_name(&self) -> &'static str {
        match &self.payload {
            SignalPayload::Typing { .. } => "Typing",
            SignalPayload::StopTyping { .. } => "StopTyping",
            SignalPayload::NewData { .. } => "NewData",
            SignalPayload::ConversationInvite { .. } => "ConversationInvite",
            SignalPayload::ConversationInviteAck { .. } => "ConversationInviteAck",
            SignalPayload::SyncNudge { .. } => "SyncNudge",
        }
    }

    pub(crate) const fn conversation_key(&self) -> &RecordKey {
        match &self.payload {
            SignalPayload::Typing { conv_key, .. }
            | SignalPayload::StopTyping { conv_key, .. }
            | SignalPayload::NewData { conv_key, .. } => conv_key,
            SignalPayload::ConversationInvite { manifest_key, .. }
            | SignalPayload::ConversationInviteAck { manifest_key, .. } => manifest_key,
            SignalPayload::SyncNudge { identity_key, .. } => identity_key,
        }
    }

    pub(crate) const fn sender_identity(&self) -> &RecordKey {
        match &self.payload {
            SignalPayload::Typing {
                sender_identity, ..
            }
            | SignalPayload::StopTyping {
                sender_identity, ..
            }
            | SignalPayload::NewData {
                sender_identity, ..
            }
            | SignalPayload::ConversationInvite {
                inviter_key: sender_identity,
                ..
            }
            | SignalPayload::ConversationInviteAck {
                invitee_key: sender_identity,
                ..
            }
            | SignalPayload::SyncNudge {
                identity_key: sender_identity,
                ..
            } => sender_identity,
        }
    }

    pub(crate) const fn sender_proof(&self) -> Option<&SenderProof> {
        self.proof.as_ref()
    }

    pub(crate) const fn has_sender_proof(&self) -> bool {
        self.proof.is_some()
    }

    /// Whether a duplicate send is harmless, so a failed fan-out may re-resolve
    /// routes and try again. `NewData` re-pulls the named subkey and an invite
    /// (or its ack) re-imports/retires from the DHT — all idempotent. Presence
    /// signals are single-shot: a replayed `Typing` resurrects an indicator the
    /// peer already cleared.
    pub(crate) const fn is_durable(&self) -> bool {
        matches!(
            self.payload,
            SignalPayload::NewData { .. }
                | SignalPayload::ConversationInvite { .. }
                | SignalPayload::ConversationInviteAck { .. }
        )
    }

    pub(crate) async fn to_signed_bytes(&self, keypair: &KeyPair) -> Result<Vec<u8>> {
        if self.proof.is_some() {
            return Ok(self.to_bytes());
        }
        let mut buf = self.unsigned_bytes();
        let proof = SenderProof::sign(&buf, keypair).await?;
        public_key_to_wire(&mut buf, &proof.sender_public_key);
        signature_to_wire(&mut buf, &proof.signature);
        Ok(buf)
    }

    pub(crate) fn unsigned_bytes(&self) -> Vec<u8> {
        unsigned_bytes(&self.payload)
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = self.unsigned_bytes();
        if let Some(proof) = self.sender_proof() {
            public_key_to_wire(&mut buf, &proof.sender_public_key);
            signature_to_wire(&mut buf, &proof.signature);
        }
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        from_bytes(bytes)
    }
}

/// Encode a payload: tag byte, first key, optional middle bytes, then tail keys.
///
/// All payloads follow: `[tag][first_key][middle_bytes...][tail_keys...]`.
fn encode_signal(
    tag: SignalTag,
    first_key: &RecordKey,
    middle: &[u8],
    tail: &[&RecordKey],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        1 + config::crypto::RECORD_KEY_WIRE_LEN * (1 + tail.len()) + middle.len(),
    );
    buf.push(tag as u8);
    key_to_wire(&mut buf, first_key);
    buf.extend_from_slice(middle);
    for key in tail {
        key_to_wire(&mut buf, key);
    }
    buf
}

fn unsigned_bytes(payload: &SignalPayload) -> Vec<u8> {
    match payload {
        SignalPayload::Typing {
            conv_key,
            sender_identity,
        } => encode_signal(SignalTag::Typing, conv_key, &[], &[sender_identity]),
        SignalPayload::StopTyping {
            conv_key,
            sender_identity,
        } => encode_signal(SignalTag::StopTyping, conv_key, &[], &[sender_identity]),
        SignalPayload::NewData {
            conv_key,
            sender_identity,
            member_subkey,
        } => encode_signal(
            SignalTag::NewData,
            conv_key,
            &member_subkey.to_le_bytes(),
            &[sender_identity],
        ),
        SignalPayload::ConversationInvite {
            manifest_key,
            inviter_key,
        } => encode_signal(
            SignalTag::ConversationInvite,
            manifest_key,
            &[],
            &[inviter_key],
        ),
        SignalPayload::ConversationInviteAck {
            manifest_key,
            inviter_key,
            invitee_key,
        } => encode_signal(
            SignalTag::ConversationInviteAck,
            manifest_key,
            &[],
            &[inviter_key, invitee_key],
        ),
        SignalPayload::SyncNudge {
            identity_key,
            scope,
            device_id,
        } => {
            let mut middle = Vec::with_capacity(3 + device_id.len());
            middle.push(*scope);
            push_u16_len_prefixed(&mut middle, device_id.as_bytes());
            encode_signal(SignalTag::SyncNudge, identity_key, &middle, &[])
        }
    }
}

fn from_bytes(bytes: &[u8]) -> Result<PeerSignal> {
    if bytes.len() < 2 {
        bail!("signal too short: {} bytes", bytes.len());
    }
    let tag = SignalTag::try_from(bytes[0]).map_err(|t| anyhow!("unknown signal tag: {t:#x}"))?;
    let (conv_key, key_end) = key_from_wire(&bytes[1..])?;
    let key_end = key_end + 1;

    // Parse the tag-specific body; the sender-proof tail is shared by every variant.
    let (payload, body_end) = match tag {
        SignalTag::Typing | SignalTag::StopTyping => {
            let (sender_identity, used) = key_from_wire(&bytes[key_end..])?;
            let payload = if matches!(tag, SignalTag::Typing) {
                SignalPayload::Typing {
                    conv_key,
                    sender_identity,
                }
            } else {
                SignalPayload::StopTyping {
                    conv_key,
                    sender_identity,
                }
            };
            (payload, key_end + used)
        }
        SignalTag::ReservedReadUpTo => bail!("reserved signal tag: 0x03"),
        SignalTag::NewData => {
            if bytes.len() < key_end + 4 {
                bail!("NewData signal too short");
            }
            let subkey_bytes: [u8; 4] = bytes[key_end..key_end + 4]
                .try_into()
                .context("member_subkey bytes")?;
            let (sender_identity, used) = key_from_wire(&bytes[key_end + 4..])?;
            (
                SignalPayload::NewData {
                    conv_key,
                    sender_identity,
                    member_subkey: u32::from_le_bytes(subkey_bytes),
                },
                key_end + 4 + used,
            )
        }
        SignalTag::ConversationInvite => {
            let (inviter_key, used) = key_from_wire(&bytes[key_end..])?;
            (
                SignalPayload::ConversationInvite {
                    manifest_key: conv_key,
                    inviter_key,
                },
                key_end + used,
            )
        }
        SignalTag::ConversationInviteAck => {
            let (inviter_key, used) = key_from_wire(&bytes[key_end..])?;
            let (invitee_identity, ack_sender_used) = key_from_wire(&bytes[key_end + used..])?;
            (
                SignalPayload::ConversationInviteAck {
                    manifest_key: conv_key,
                    inviter_key,
                    invitee_key: invitee_identity,
                },
                key_end + used + ack_sender_used,
            )
        }
        SignalTag::SyncNudge => sync_nudge_from_wire(bytes, key_end, conv_key)?,
    };

    let proof = sender_proof_from_wire(&bytes[body_end..])?;
    Ok(PeerSignal {
        payload,
        proof: Some(proof),
    })
}

/// Decodes the `SyncNudge` middle (`[scope u8][device_id_len u16 LE]
/// [device_id]`) and returns it with the offset where the shared
/// sender-proof tail begins.
fn sync_nudge_from_wire(
    bytes: &[u8],
    key_end: usize,
    identity_key: RecordKey,
) -> Result<(SignalPayload, usize)> {
    if bytes.len() < key_end + 3 {
        bail!("SyncNudge signal too short");
    }
    let scope = *bytes.get(key_end).context("SyncNudge scope byte")?;
    let (device_bytes, device_end) =
        read_u16_len_prefixed(bytes, key_end + 1, "SyncNudge device id")?;
    let device_id: Arc<str> = str::from_utf8(device_bytes)
        .context("SyncNudge device id is not valid UTF-8")?
        .into();
    Ok((
        SignalPayload::SyncNudge {
            identity_key,
            scope,
            device_id,
        },
        device_end,
    ))
}
