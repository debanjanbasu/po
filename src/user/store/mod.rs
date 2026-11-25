//! Local accounts, wrapped credentials, encrypted rotation history, and session metadata.
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    str,
};

use anyhow::{Result, anyhow, bail, ensure};
use bytes::Bytes;
use tokio::sync::Mutex;
use veilid_core::{BareSharedSecret, CryptoKind, SharedSecret, VALID_CRYPTO_KINDS};

use crate::{
    config::{self, storage::DEVICE_TABLE},
    helpers::{
        async_utils::now_epoch_secs,
        encryption::{decrypt_with_nonce, encrypt_with_nonce, get_cached_crypto_system},
        store_lock::lock_store,
    },
    user::{User, load_user},
    veilid::table_store::Table,
};

pub mod cleanup;

pub use cleanup::{
    CLEANUP_TABLE, delete_all_user_data, is_cleanup_pending, retry_incomplete_cleanups,
};

pub const USER_TABLE: &str = "po_users";
pub const USER_TABLE_COLS: u32 = 3;
pub(super) const COL_USER: u32 = 0;
pub(super) const COL_PASSWORD_HASH: u32 = 1;
pub(super) const COL_PASSWORD_ROTATION: u32 = 2;

pub(super) const SESSION_TABLE: &str = "po_session";
pub(super) const SESSION_COL: u32 = 0;
pub(super) const LAST_ACTIVE_KEY: &[u8] = b"last_active";
const GUEST_THEME_KEY: &[u8] = b"guest_theme";
pub(super) const PIN_ATTEMPTS_PREFIX: &[u8] = b"pin_attempts:";
pub(super) const PAIRING_ATTEMPTS_PREFIX: &[u8] = b"pairing_attempts:";

/// Fixed width of the suffix an append-only attempt row adds to its identity
/// key: an 8-byte big-endian attempt timestamp, so byte order is age order, and
/// 4 random bytes, so two processes failing in the same second write distinct
/// rows instead of overwriting one another's.
const ATTEMPT_TOKEN_LEN: usize = 12;
/// Marker value for an attempt row; the timestamp lives in the key.
const ATTEMPT_ROW_VALUE: &[u8] = &[1];
/// Rows retained per identity, past [`super::password::exponential_cooldown_secs`]'s
/// saturation point (`1 << 10` from the 11th threshold multiple): keeping more
/// cannot change a cooldown, and the cap bounds the scan every attempt makes.
const fn attempt_row_cap() -> usize {
    config::pin_gate::LOCKOUT_THRESHOLD as usize * 12
}

fn is_supported_theme(theme: &str) -> bool {
    matches!(theme, "system" | "light" | "dark")
}

pub(super) async fn users_table() -> Result<Table> {
    Table::open(USER_TABLE, USER_TABLE_COLS).await
}

pub(super) async fn session_table() -> Result<Table> {
    Table::open(SESSION_TABLE, 1).await
}

/// Packs and persists a `User` to local storage.
pub async fn store_user(user: &super::User) -> Result<()> {
    users_table()
        .await?
        .store(
            COL_USER,
            user.username().as_bytes(),
            &user.to_packed_capnp()?,
        )
        .await
}

/// Load only the still-live account captured by an asynchronous operation.
/// Durable callers hold the username store lock through the ensuing commit.
pub async fn load_matching_user(username: &str, account_id: &str) -> Result<Option<User>> {
    ensure!(!account_id.is_empty(), "Account ID is unavailable");
    let Some(user) = load_user(username)
        .await?
        .filter(|user| user.account_id() == account_id)
    else {
        return Ok(None);
    };
    if cleanup::is_cleanup_pending(account_id).await? {
        return Ok(None);
    }
    Ok(Some(user))
}

pub async fn load_password_hash_opt(username: &str) -> Result<Option<Vec<u8>>> {
    let Some(stored) = users_table()
        .await?
        .load_opt(COL_PASSWORD_HASH, username.as_bytes())
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(unwrap_password_hash(&stored).await?))
}

pub async fn store_password_hash(username: &str, password_hash: &[u8]) -> Result<()> {
    let wrapped = wrap_password_hash(password_hash).await?;
    users_table()
        .await?
        .store(COL_PASSWORD_HASH, username.as_bytes(), &wrapped)
        .await
}

pub(super) async fn load_password_rotation(account_id: &str) -> Result<Option<Bytes>> {
    users_table()
        .await?
        .load_opt(COL_PASSWORD_ROTATION, account_id.as_bytes())
        .await
}

/// The keypair, optional remembered credential, and encrypted migration
/// history are one durable commit, before handing work to the sync queue.
pub(super) async fn store_password_change(
    user: &User,
    password_hash: Option<&[u8]>,
    rotation: &[u8],
) -> Result<()> {
    let wrapped = match password_hash {
        Some(hash) => Some(wrap_password_hash(hash).await?),
        None => None,
    };
    let user_bytes = user.to_packed_capnp()?;
    let key = user.username().as_bytes();
    let mut stores = vec![
        (COL_USER, key, user_bytes.as_slice()),
        (
            COL_PASSWORD_ROTATION,
            user.account_id().as_bytes(),
            rotation,
        ),
    ];
    if let Some(wrapped) = wrapped.as_deref() {
        stores.push((COL_PASSWORD_HASH, key, wrapped));
    }
    users_table().await?.apply_batch(&[], &stores).await
}

pub async fn delete_password_hash(username: &str) -> Result<()> {
    users_table()
        .await?
        .delete(COL_PASSWORD_HASH, username.as_bytes())
        .await
}

pub async fn store_last_active(username: &str) -> Result<()> {
    session_table()
        .await?
        .store(SESSION_COL, LAST_ACTIVE_KEY, username.as_bytes())
        .await
}

pub async fn load_last_active() -> Result<Option<String>> {
    let Some(bytes) = session_table()
        .await?
        .load_opt(SESSION_COL, LAST_ACTIVE_KEY)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(String::from_utf8(bytes.to_vec())?))
}

pub async fn clear_last_active() -> Result<()> {
    session_table()
        .await?
        .delete(SESSION_COL, LAST_ACTIVE_KEY)
        .await
}

pub async fn load_guest_theme_opt() -> Result<Option<String>> {
    let Some(bytes) = session_table()
        .await?
        .load_opt(SESSION_COL, GUEST_THEME_KEY)
        .await?
    else {
        return Ok(None);
    };
    let theme = str::from_utf8(&bytes)?;
    if !is_supported_theme(theme) {
        bail!("unsupported persisted theme value: {theme}");
    }
    Ok(Some(theme.to_owned()))
}

pub async fn store_guest_theme(theme: &str) -> Result<()> {
    if !is_supported_theme(theme) {
        bail!("unsupported theme value: {theme}");
    }
    session_table()
        .await?
        .store(SESSION_COL, GUEST_THEME_KEY, theme.as_bytes())
        .await
}

/// Outcome of one brute-force-gated credential attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Verification succeeded; the failure counter was cleared.
    Verified,
    /// Verification failed below the lockout threshold; counter incremented.
    Rejected { attempts: u32 },
    /// Lockout threshold reached — attempts are refused until `until`.
    Locked { attempts: u32, until: u64 },
}

/// Runs one verification attempt under durable brute-force gating.
///
/// Loads the attempt state for `key`, refuses while locked out, runs `verify`,
/// then records the failure as an append-only row (locking at the threshold) or
/// clears every row on success.
///
/// The caller holding the per-user store lock keeps *this* process's attempts
/// ordered; the append-only rows are what make attempts from *other* processes
/// count too — an incremented counter row would let two processes read the same
/// count and overwrite each other's increment.
pub async fn gated_attempt(
    key: &[u8],
    verify: impl AsyncFnOnce() -> Result<bool>,
) -> Result<AttemptOutcome> {
    let now = now_epoch_secs();
    let state = load_attempt_state_by_key(key).await?;
    if state.locked_until > now {
        return Ok(AttemptOutcome::Locked {
            attempts: state.count,
            until: state.locked_until,
        });
    }

    if verify().await? {
        clear_attempt_state_by_key(key).await?;
        return Ok(AttemptOutcome::Verified);
    }

    // Record before returning: the row is the attempt, written under a key no
    // other process can collide with, so two processes that both read the same
    // count and both fail each add one row and both are counted.
    let state = record_attempt(key, now).await?;
    if state.locked_until > now {
        return Ok(AttemptOutcome::Locked {
            attempts: state.count,
            until: state.locked_until,
        });
    }
    Ok(AttemptOutcome::Rejected {
        attempts: state.count,
    })
}

pub(super) fn pin_key(username: &str) -> Vec<u8> {
    [PIN_ATTEMPTS_PREFIX, username.as_bytes()].concat()
}

pub(super) fn pairing_attempt_key(nonce_bytes: &[u8]) -> Vec<u8> {
    let h = {
        let mut hasher = DefaultHasher::new();
        nonce_bytes.hash(&mut hasher);
        hasher.finish()
    };
    let mut key = Vec::with_capacity(PAIRING_ATTEMPTS_PREFIX.len() + 8);
    key.extend_from_slice(PAIRING_ATTEMPTS_PREFIX);
    key.extend_from_slice(&h.to_le_bytes());
    key
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PinAttemptState {
    pub count: u32,
    pub locked_until: u64,
}

/// Loads one attempt row's value.
async fn load_attempt_row(table: &Table, key: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(table
        .load_opt(SESSION_COL, key)
        .await?
        .map(|bytes| bytes.to_vec()))
}

/// Stores one attempt row's value under the session table key.
async fn store_attempt_row(table: &Table, key: &[u8], value: &[u8]) -> Result<()> {
    table.store(SESSION_COL, key, value).await
}

/// Deletes one attempt row under the session table key.
async fn delete_attempt_row(table: &Table, key: &[u8]) -> Result<()> {
    table.delete(SESSION_COL, key).await
}

/// Reads the attempt rows for `key`, oldest first.
///
/// A row belongs to `key` iff it is exactly `key ‖ ATTEMPT_TOKEN_LEN` bytes: the
/// token is fixed-width, so a different identity could only masquerade by being
/// a strict prefix of this one *and* having the same total length — impossible
/// unless it is this key (an equal-length identity cannot be a strict prefix).
async fn attempt_rows(table: &Table, key: &[u8]) -> Result<Vec<(u64, Vec<u8>)>> {
    let mut rows = Vec::new();
    for candidate in table.raw().get_keys(SESSION_COL).await? {
        if candidate.len() == key.len() + ATTEMPT_TOKEN_LEN && candidate.starts_with(key) {
            let ts = u64::from_be_bytes(candidate[key.len()..key.len() + 8].try_into()?);
            rows.push((ts, candidate.to_vec()));
        }
    }
    rows.sort_by_key(|(ts, _)| *ts);
    Ok(rows)
}

/// Records one failed attempt for `key` under a key no other process can collide
/// with, trims rows past the cap, and returns the merged state.
///
/// Rows are only ever added, so two processes that both read count `n` and both
/// fail write two rows: the count is the number of rows, and no attempt is lost.
/// The pre-append-only counter row — read, never written, by
/// [`load_attempt_state_by_key`] — stays untouched.
async fn record_attempt(key: &[u8], now: u64) -> Result<PinAttemptState> {
    let mut token = [0u8; ATTEMPT_TOKEN_LEN];
    token[..8].copy_from_slice(&now.to_be_bytes());
    token[8..].copy_from_slice(&get_cached_crypto_system().await?.random_nonce().bytes()[..4]);
    let mut row = key.to_vec();
    row.extend_from_slice(&token);

    let table = session_table().await?;
    store_attempt_row(&table, &row, ATTEMPT_ROW_VALUE).await?;

    let mut rows = attempt_rows(&table, key).await?;
    while rows.len() > attempt_row_cap() {
        let (_, stale) = rows.remove(0);
        delete_attempt_row(&table, &stale).await?;
    }
    attempt_state(key, &rows).await
}

/// Cooldown window an attempt at `ts` puts a count of `count` under, or 0.
fn locked_until_for(count: u32, ts: u64) -> u64 {
    let cooldown = super::password::exponential_cooldown_secs(count);
    if cooldown > 0 && count.is_multiple_of(config::pin_gate::LOCKOUT_THRESHOLD) {
        ts.saturating_add(cooldown)
    } else {
        0
    }
}

/// Merges the legacy in-place counter row with the append-only rows for `key`.
///
/// The legacy row — `{prefix}:{identity}`, `count ‖ locked_until` — is read
/// until no build that writes it can be in the field *and*
/// [`config::pin_gate::MAX_COOLDOWN_SECS`] has elapsed since the last write:
/// dropping the read early would silently reset a lockout that is currently in
/// force, which would turn this fix into a regression.
async fn attempt_state(key: &[u8], rows: &[(u64, Vec<u8>)]) -> Result<PinAttemptState> {
    let legacy = load_attempt_row(&session_table().await?, key).await?;
    let mut state = legacy.map_or_else(PinAttemptState::default, |bytes| PinAttemptState {
        count: bytes
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .map_or(0, u32::from_le_bytes),
        locked_until: bytes
            .get(4..12)
            .and_then(|b| b.try_into().ok())
            .map_or(0, u64::from_le_bytes),
    });
    for &(ts, _) in rows {
        state.count = state.count.saturating_add(1);
        state.locked_until = state.locked_until.max(locked_until_for(state.count, ts));
    }
    Ok(state)
}

async fn load_attempt_state_by_key(key: &[u8]) -> Result<PinAttemptState> {
    let table = session_table().await?;
    let rows = attempt_rows(&table, key).await?;
    attempt_state(key, &rows).await
}

/// Deletes the legacy counter row and every attempt row for `key`.
async fn clear_attempt_state_by_key(key: &[u8]) -> Result<()> {
    let table = session_table().await?;
    delete_attempt_row(&table, key).await?;
    for (_, row) in attempt_rows(&table, key).await? {
        delete_attempt_row(&table, &row).await?;
    }
    Ok(())
}

/// Loads persisted PIN brute-force state for a given username.
pub async fn load_pin_attempt_state(username: &str) -> Result<PinAttemptState> {
    load_attempt_state_by_key(&pin_key(username)).await
}

// ── Device wrap key (remember-me password hash encryption) ─────────────
//
// The key below needs no app-level wrap of its own: `TableDB` already
// AEAD-encrypts every stored key and value under a device key held in the OS
// protected store (Keychain / Keystore / Credential Manager).

pub(super) const DEVICE_KEY_KEY: &[u8] = b"device_key";
pub(super) const DEVICE_KEY_KIND_KEY: &[u8] = b"device_key_kind";
pub(super) const DEVICE_WRAP_KEY_LOCK_KEY: &[u8] = b"user.store.device_wrap_key";

/// Process-local cache for the persistent remember-me device wrap key.
///
/// The underlying key still lives in `po_device`, but wrap/unwrap callers only
/// re-enter `TableDB` on the first use of the current app session. Final-account
/// cleanup clears the cache when it removes the device rows.
static DEVICE_WRAP_KEY: Mutex<Option<SharedSecret>> = Mutex::const_new(None);

async fn load_cached_device_wrap_key() -> Option<SharedSecret> {
    DEVICE_WRAP_KEY.lock().await.clone()
}

pub(super) async fn clear_cached_device_wrap_key() {
    *DEVICE_WRAP_KEY.lock().await = None;
}

/// Gets or creates a per-device random 32-byte key for wrapping stored password hashes.
async fn get_or_create_device_key() -> Result<SharedSecret> {
    if let Some(secret) = load_cached_device_wrap_key().await {
        return Ok(secret);
    }

    let _guard = lock_store(DEVICE_WRAP_KEY_LOCK_KEY).await;
    if let Some(secret) = load_cached_device_wrap_key().await {
        return Ok(secret);
    }

    let table = Table::open(DEVICE_TABLE, 1).await?;
    let secret = if let Some(raw) = table.load_opt(0, DEVICE_KEY_KEY).await?
        && raw.len() == config::crypto::KEY_LEN
    {
        let kind = match table.load_opt(0, DEVICE_KEY_KIND_KEY).await? {
            Some(kind) if kind.len() == config::crypto::CRYPTO_KIND_LEN => CryptoKind::new(
                <[u8; 4]>::try_from(&kind[..])
                    .map_err(|_| anyhow!("invalid stored device-key kind"))?,
            ),
            _ => VALID_CRYPTO_KINDS[0],
        };
        SharedSecret::new(kind, BareSharedSecret::new(&raw))
    } else {
        let cs = get_cached_crypto_system().await?;
        let secret = cs.random_shared_secret();
        table
            .store(0, DEVICE_KEY_KEY, &secret.value().bytes())
            .await?;
        table
            .store(0, DEVICE_KEY_KIND_KEY, secret.kind().bytes())
            .await?;
        secret
    };

    *DEVICE_WRAP_KEY.lock().await = Some(secret.clone());
    Ok(secret)
}

/// AEAD-encrypts a password hash with the device key before persisting.
pub(super) async fn wrap_password_hash(hash: &[u8]) -> Result<Vec<u8>> {
    let device_key = get_or_create_device_key().await?;
    let cs = get_cached_crypto_system().await?;
    encrypt_with_nonce(hash, &device_key, &cs)
}

/// AEAD-decrypts a device-wrapped password hash.
pub(super) async fn unwrap_password_hash(wrapped: &[u8]) -> Result<Vec<u8>> {
    let device_key = get_or_create_device_key().await?;
    let cs = get_cached_crypto_system().await?;
    decrypt_with_nonce(wrapped, &device_key, &cs)
}
