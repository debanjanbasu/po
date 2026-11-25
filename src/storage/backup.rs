//! Encrypted backup export and import.
//!
//! **Format:**
//! ```text
//! [4B magic "POBK"][4B version u32 LE]
//! [remaining: nonce-prefixed AEAD ciphertext]
//! ```
//!
//! - **v3**: zstd-compressed payload; AEAD header binding and account-bound
//!   v2 DHT-secret derivation. Export writes and import accepts v3 only.
//!
//! Backup encryption uses the session's account-bound `dht_secret` (password-derived
//! via Argon2 + app pepper). Export is zero-prompt (key already in memory). Import
//! requires the active account ID and the password used when exporting.
//!
//! Section payload:
//! ```text
//! [4B section_count u32 LE]
//! per section:
//!   [1B model_id_len][model_id bytes][4B data_len u32 LE][data bytes]
//! ```
//!
//! Compression uses `zstd` directly — deliberately independent of the DHT
//! codec (no dictionaries, no custom framing), so backups stay decodable
//! without any device-local dictionary state. Payloads at or above
//! [`config::backup::MT_THRESHOLD_BYTES`] compress with `zstdmt` worker
//! threads (see `docs/compression.md`).

#[cfg(not(target_arch = "wasm32"))]
use std::thread;
use std::{collections::HashSet, str};

use anyhow::{Context, Result, anyhow, bail, ensure};
use thiserror::Error;
use veilid_core::SharedSecret;
use zeroize::Zeroizing;
use zstd::bulk;
#[cfg(not(target_arch = "wasm32"))]
use zstd::zstd_safe::CParameter;
use zstd::zstd_safe::get_frame_content_size;

use crate::{
    config,
    helpers::{
        async_utils::spawn_blocking,
        capnp::capnp_u32,
        encryption::{
            PasswordHashForm, crypto_system_sync, decrypt_with_nonce_aad,
            derive_dht_secret_from_password, encrypt_with_nonce_aad,
        },
        framing,
    },
};

const MAGIC: &[u8; 4] = b"POBK";
/// v3: zstd-compressed payload and account-bound KDF.
const VERSION_V3: u32 = 3;

/// User-facing import failure categories. Attached to the `anyhow` chain so
/// the UI can `downcast_ref` instead of string-matching error text. The
/// `Display` strings double as the user-visible messages.
#[derive(Debug, Error)]
pub enum ImportError {
    // Names all three causes, because the format cannot separate them: the
    // header binds only magic and version, the account id lives *inside* the
    // payload, and the KDF takes the id as an input — so a backup from a
    // different account fails to decrypt here, before `verify_account_binding`
    // can say "different account". That matters for the common case of a
    // reinstall: the account id is minted randomly at signup, so signing up
    // again rather than pairing yields a new id and the user's own old backup
    // refuses. Telling them only "wrong password" makes them retype it forever.
    #[error("Wrong password, different account, or corrupted backup file")]
    WrongPassword,
    #[error("Not a valid backup file")]
    InvalidFile,
    #[error("Unsupported backup version {0}")]
    UnsupportedVersion(u32),
    // Names both numbers: the web build caps a file at a fraction of the native
    // cap, so "too large" without them reads as corruption to a user whose file
    // the desktop build exported happily.
    #[error(
        "This backup is {file_mb} MB, and the largest one this {platform} can open is {limit_mb} \
         MB. Use a build without that limit (desktop has none), or reduce the history it carries."
    )]
    TooLarge {
        file_mb: u64,
        limit_mb: u64,
        platform: &'static str,
    },
}

/// The typed refusal for a backup file above this platform's cap.
const fn too_large(file_bytes: u64) -> ImportError {
    ImportError::TooLarge {
        file_mb: file_bytes / BYTES_PER_MB,
        limit_mb: config::backup::MAX_FILE_BYTES / BYTES_PER_MB,
        platform: SIZE_LIMIT_PLATFORM,
    }
}

/// The same refusal as a sentence, for the file picker that checks the size
/// *before* reading the file.
///
/// One constructor for both check sites, because a second wording drifts: the
/// picker used to call an oversized file "too large to be a valid backup", which
/// tells the user their file is corrupt when the truth is that this platform is
/// smaller than the one that wrote it — and they act differently on those.
#[must_use]
pub fn too_large_message(file_bytes: u64) -> String {
    too_large(file_bytes).to_string()
}

/// The largest this platform can produce and re-open.
///
/// Named in the refusals so the two halves of the limit — this build exports it,
/// this build must be able to read it back — are visible to the user.
const SIZE_LIMIT_PLATFORM: &str = if cfg!(target_arch = "wasm32") {
    "browser"
} else {
    "build"
};

/// Backup export/import sizes are measured in whole megabytes in every message.
const BYTES_PER_MB: u64 = 1024 * 1024;

/// User-facing export failure categories, attached to the `anyhow` chain the
/// way [`ImportError`] is: the UI prints the `Display` string, so it names the
/// limit and the way out instead of a size the user cannot act on.
#[derive(Debug, Error)]
pub enum ExportError {
    #[error(
        "This backup is {bytes_mb} MB, and the largest one this {platform} can open again is \
         {limit_mb} MB. Export it from the desktop app, or reduce the history it carries."
    )]
    TooLarge {
        bytes_mb: u64,
        limit_mb: u64,
        platform: &'static str,
    },
}

/// AEAD associated data binding the plaintext header (magic + version) to the
/// ciphertext. Without this, flipping the version byte would still decrypt and
/// misparse the payload.
fn header_aad(version: u32) -> [u8; 8] {
    let mut aad = [0u8; 8];
    aad[..4].copy_from_slice(MAGIC);
    aad[4..].copy_from_slice(&version.to_le_bytes());
    aad
}

/// Model ID for the identity+keypair section in a backup.
pub const IDENTITY_SECTION_ID: &str = "identity";

/// Section ID for per-conversation message data (keyed list of packed
/// `LocalConversation` snapshots, encoded via [`encode_kv_list`]).
pub const CONVERSATIONS_DATA_SECTION_ID: &str = "conversations_data";

/// A named section within a backup bundle.
pub struct BackupSection {
    pub model_id: &'static str,
    pub data: Vec<u8>,
}

/// Serialized identity snapshot included in backups.
///
/// Contains everything needed to fully restore a user's identity on a new device.
pub struct IdentityBackup {
    pub username: String,
    pub dht_key: String,
    pub account_id: String,
    /// AEAD nonce for the encrypted keypair (24 bytes).
    pub keypair_nonce: [u8; 24],
    /// AEAD-encrypted keypair ciphertext (encrypted with `dht_secret`).
    pub keypair_ciphertext: Vec<u8>,
}

impl IdentityBackup {
    /// Encode to binary for inclusion in a backup section.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        framing::push_str(&mut buf, &self.username);
        framing::push_str(&mut buf, &self.dht_key);
        framing::push_str(&mut buf, &self.account_id);
        buf.extend_from_slice(&self.keypair_nonce);
        buf.extend_from_slice(&self.keypair_ciphertext);
        buf
    }

    /// Decode from binary backup section data.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let username = r.str("identity username")?.to_string();
        let dht_key = r.str("identity dht key")?.to_string();
        let account_id = r.str("identity account id")?.to_string();
        let keypair_nonce: [u8; 24] = r.take(24, "identity nonce")?.try_into()?;
        let keypair_ciphertext = r.take(r.remaining(), "identity keypair")?.to_vec();
        if keypair_ciphertext.is_empty() {
            bail!("Identity section has empty keypair ciphertext");
        }
        Ok(Self {
            username,
            dht_key,
            account_id,
            keypair_nonce,
            keypair_ciphertext,
        })
    }
}

/// Bounds-checked sequential reader over untrusted backup bytes.
/// All arithmetic is overflow-safe (relevant on 32-bit wasm).
struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(n)
            .filter(|&end| end <= self.data.len())
            .with_context(|| format!("Backup data truncated ({what})"))?;
        let slice = &self.data[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self, what: &str) -> Result<u8> {
        Ok(self.take(1, what)?[0])
    }

    fn u32(&mut self, what: &str) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4, what)?.try_into()?))
    }

    fn str(&mut self, what: &str) -> Result<&'a str> {
        let len = self.u32(what)? as usize;
        str::from_utf8(self.take(len, what)?)
            .with_context(|| format!("Invalid UTF-8 in backup data ({what})"))
    }

    const fn remaining(&self) -> usize {
        self.data.len() - self.offset
    }

    /// Rejects trailing bytes after the last expected element.
    fn finish(&self, what: &str) -> Result<()> {
        ensure!(
            self.offset == self.data.len(),
            "Backup data has trailing bytes ({what})"
        );
        Ok(())
    }
}

/// Produces an encrypted backup file from the given sections.
///
/// Uses the session's pre-derived `dht_secret` — no password prompt needed.
/// The payload is zstd-compressed before encryption (format v3); payloads at
/// or above [`config::backup::MT_THRESHOLD_BYTES`] use multithreaded zstd.
///
/// Refuses a payload this platform could not open again
/// ([`ExportError::TooLarge`]) *before* the payload, compressed and ciphertext
/// copies exist at once. On web the compression itself runs inline on the UI
/// thread (`spawn_blocking` is a label there), so an unbounded export is a
/// multi-second freeze followed by the chance of a heap trap, and the file it
/// produced would not import anyway.
pub async fn export_backup(secret: &SharedSecret, sections: Vec<BackupSection>) -> Result<Vec<u8>> {
    let payload_bytes = encoded_len(&sections);
    ensure!(
        payload_bytes <= config::backup::MAX_DECOMPRESSED_BYTES,
        ExportError::TooLarge {
            bytes_mb: payload_bytes / BYTES_PER_MB,
            limit_mb: config::backup::MAX_DECOMPRESSED_BYTES / BYTES_PER_MB,
            platform: SIZE_LIMIT_PLATFORM,
        }
    );

    let payload = Zeroizing::new(encode_payload(&sections));

    let ciphertext = spawn_blocking({
        let secret = secret.clone();
        move || {
            let compressed = Zeroizing::new(compress_payload(&payload)?);
            let cs = crypto_system_sync()?;
            encrypt_with_nonce_aad(&compressed, &secret, &cs, Some(&header_aad(VERSION_V3)))
        }
    })
    .await??;

    let mut out = Vec::with_capacity(4 + 4 + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION_V3.to_le_bytes());
    out.extend(ciphertext);
    Ok(out)
}

/// Decrypts a backup file and returns its sections.
///
/// Re-derives the account-bound encryption key from the password and active
/// account ID (the session secret isn't needed during import).
pub async fn import_backup(
    password: &str,
    account_id: &str,
    file: &[u8],
) -> Result<Vec<(String, Vec<u8>)>> {
    let ciphertext = parse_header(file)?;

    // The backup format does not record which salt form its account hashed the
    // password with, and nothing else on disk does either: the header decrypt
    // is the oracle. It has to be — a derivation succeeds under *any* form, so
    // picking on the derivation alone would always take the newest one and
    // reject every backup exported before the account-ID salt as a wrong
    // password. Try each form, newest first, and keep the one that opens the
    // file; a legacy file simply pays one extra Argon2 and AEAD attempt.
    let mut plaintext = None;
    for form in PasswordHashForm::NEWEST_FIRST {
        let secret = derive_dht_secret_from_password(password, account_id, form).await?;
        let opened = spawn_blocking({
            let ciphertext = ciphertext.to_vec();
            move || -> Result<Option<Zeroizing<Vec<u8>>>> {
                let cs = crypto_system_sync()?;
                let Ok(decrypted) = decrypt_with_nonce_aad(
                    &ciphertext,
                    &secret,
                    &cs,
                    Some(&header_aad(VERSION_V3)),
                ) else {
                    // Wrong form (or wrong password): the next form's attempt,
                    // or the caller's WrongPassword below, is the verdict.
                    return Ok(None);
                };
                Ok(Some(Zeroizing::new(decompress_payload(&decrypted)?)))
            }
        })
        .await??;
        if let Some(opened) = opened {
            plaintext = Some(opened);
            break;
        }
    }
    let plaintext = plaintext.context(ImportError::WrongPassword)?;

    decode_payload(&plaintext)
}

fn parse_header(file: &[u8]) -> Result<&[u8]> {
    ensure!(
        u64::try_from(file.len()).is_ok_and(|len| len <= config::backup::MAX_FILE_BYTES),
        too_large(u64::try_from(file.len()).unwrap_or(u64::MAX))
    );
    if file.len() < 8 || &file[..4] != MAGIC {
        bail!(ImportError::InvalidFile);
    }
    let version = u32::from_le_bytes(file[4..8].try_into()?);
    if version != VERSION_V3 {
        bail!(ImportError::UnsupportedVersion(version));
    }
    Ok(&file[8..])
}

/// Compresses a backup payload with zstd. Large payloads engage `zstdmt`
/// worker threads (native targets only — the wasm build has no threads).
fn compress_payload(payload: &[u8]) -> Result<Vec<u8>> {
    let mut compressor = bulk::Compressor::new(config::compression::ZSTD_LEVEL)?;

    #[cfg(not(target_arch = "wasm32"))]
    if payload.len() >= config::backup::MT_THRESHOLD_BYTES {
        let workers = thread::available_parallelism()
            .map_or(1, |n| capnp_u32(n.get()))
            .min(config::backup::MT_MAX_WORKERS);
        if workers > 1 {
            compressor.set_parameter(CParameter::NbWorkers(workers))?;
        }
    }

    Ok(compressor.compress(payload)?)
}

/// Decompresses a v2 backup payload, bounding allocation by the frame's
/// declared content size (always present — export compresses in one shot).
fn decompress_payload(data: &[u8]) -> Result<Vec<u8>> {
    let content_size = get_frame_content_size(data)
        .map_err(|_| anyhow!("Corrupted backup: invalid compressed data"))?
        .context("Corrupted backup: unknown decompressed size")?;
    ensure!(
        content_size <= config::backup::MAX_DECOMPRESSED_BYTES,
        "This backup unpacks to {} MB, more than the {} MB this build can open",
        content_size / BYTES_PER_MB,
        config::backup::MAX_DECOMPRESSED_BYTES / BYTES_PER_MB
    );
    Ok(bulk::decompress(
        data,
        usize::try_from(content_size).unwrap_or(usize::MAX),
    )?)
}

/// Encodes a list of `(key, bytes)` pairs — used for the per-conversation
/// data section where keys are dynamic record keys.
///
/// Layout: `[4B count u32 LE]` then per item
/// `[4B key_len][key bytes][4B data_len][data bytes]`.
#[must_use]
pub fn encode_kv_list(items: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    framing::push_u32_len_prefixed(&mut buf, &capnp_u32(items.len()).to_le_bytes());
    for (key, data) in items {
        framing::push_str(&mut buf, key);
        framing::push_u32_len_prefixed(&mut buf, data);
    }
    buf
}

/// Decodes a list encoded by [`encode_kv_list`].
/// Rejects empty, NUL-containing, and duplicate keys, and trailing bytes.
pub fn decode_kv_list(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut r = Reader::new(data);
    let count = r.u32("kv count")? as usize;
    // Hard domain cap plus a structural bound (each item needs at least its
    // two length prefixes) — both checked before any allocation.
    ensure!(count <= config::backup::MAX_KV_ITEMS, "KV list too large");
    ensure!(count <= r.remaining() / 8, "KV list count implausible");
    let cap = count.min(1024);
    let mut seen: HashSet<&str> = HashSet::with_capacity(cap);
    let mut items: Vec<(String, Vec<u8>)> = Vec::with_capacity(cap);
    for _ in 0..count {
        let key = r.str("kv key")?;
        // Storage keys use `\0` as the account separator — a key containing
        // one could escape its account prefix.
        ensure!(
            !key.is_empty() && !key.contains('\0'),
            "KV list has invalid key"
        );
        ensure!(seen.insert(key), "KV list has duplicate key");
        let data_len = r.u32("kv data length")? as usize;
        let value = r.take(data_len, "kv data")?.to_vec();
        items.push((key.to_string(), value));
    }
    r.finish("kv list")?;
    Ok(items)
}

/// The byte length [`encode_payload`] will produce, without producing it.
///
/// The export size check runs on this: measuring the encoded payload costs one
/// whole copy of every section, which is the allocation the check exists to
/// avoid. Saturating, because the only caller compares against a cap.
fn encoded_len(sections: &[BackupSection]) -> u64 {
    sections.iter().fold(4_u64, |total, section| {
        let id_len = u64::try_from(section.model_id.len()).unwrap_or(u64::MAX);
        let data_len = u64::try_from(section.data.len()).unwrap_or(u64::MAX);
        // section count (4) + per section: id-length byte (1), id, data length
        // (4), data — the layout `encode_payload` writes.
        total
            .saturating_add(5)
            .saturating_add(id_len)
            .saturating_add(data_len)
    })
}

fn encode_payload(sections: &[BackupSection]) -> Vec<u8> {
    let mut buf = Vec::new();
    framing::push_u32_len_prefixed(&mut buf, &capnp_u32(sections.len()).to_le_bytes());
    for section in sections {
        let id_bytes = section.model_id.as_bytes();
        buf.push(u8::try_from(id_bytes.len()).unwrap_or(u8::MAX));
        buf.extend_from_slice(id_bytes);
        framing::push_u32_len_prefixed(&mut buf, &section.data);
    }
    buf
}

fn decode_payload(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut r = Reader::new(data);
    let count = r.u32("section count")? as usize;
    // Hard domain cap plus a structural bound (each section needs at least
    // its id-length byte + data-length u32) — checked before any allocation.
    ensure!(
        count <= config::backup::MAX_SECTIONS,
        "Backup has too many sections"
    );
    ensure!(count <= r.remaining() / 5, "Section count implausible");
    let mut sections: Vec<(String, Vec<u8>)> = Vec::with_capacity(count);

    let mut seen_ids: HashSet<&str> = HashSet::with_capacity(count.min(1024));
    for _ in 0..count {
        let id_len = r.u8("model id length")? as usize;
        let model_id = str::from_utf8(r.take(id_len, "model id")?).context("Invalid model ID")?;
        ensure!(
            seen_ids.insert(model_id),
            "Backup has duplicate section: {model_id}"
        );
        let data_len = r.u32("section data length")? as usize;
        let section_data = r.take(data_len, "section data")?.to_vec();
        sections.push((model_id.to_string(), section_data));
    }
    r.finish("backup payload")?;
    Ok(sections)
}
