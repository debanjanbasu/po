//! TOTP token models, storage, and sync utilities.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use autosurgeon::{Hydrate, Reconcile};
use thiserror::Error;
use totp_rs::{Algorithm, Builder, Secret, Totp, TotpError};

use crate::{
    helpers::{
        async_utils::{now_epoch_millis, now_epoch_secs},
        capnp::{capnp_u32, pack, unpack},
    },
    models::{generated::totp_capnp, pending_operation::OpType},
    storage::store_trait::impl_crdt_model,
};

/// Hash algorithm for TOTP token generation.
///
/// Variant names match the strings stored in automerge,
/// preserving backward compatibility with existing data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Reconcile, Hydrate)]
pub enum TotpAlgorithm {
    SHA1,
    SHA256,
    SHA512,
}

impl TotpAlgorithm {
    /// String representation matching legacy Cap'n Proto `Text` field values.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SHA1 => "SHA1",
            Self::SHA256 => "SHA256",
            Self::SHA512 => "SHA512",
        }
    }

    /// Parse from a stored string, defaulting to SHA1 for unknown values.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "SHA256" => Self::SHA256,
            "SHA512" => Self::SHA512,
            _ => Self::SHA1,
        }
    }
}

impl From<Algorithm> for TotpAlgorithm {
    fn from(a: Algorithm) -> Self {
        match a {
            Algorithm::SHA256 => Self::SHA256,
            Algorithm::SHA512 => Self::SHA512,
            // `Algorithm` is non-exhaustive (e.g. feature-gated Steam variant);
            // fall back to the RFC default like `from_str_lossy`.
            _ => Self::SHA1,
        }
    }
}

impl From<TotpAlgorithm> for Algorithm {
    fn from(a: TotpAlgorithm) -> Self {
        match a {
            TotpAlgorithm::SHA1 => Self::SHA1,
            TotpAlgorithm::SHA256 => Self::SHA256,
            TotpAlgorithm::SHA512 => Self::SHA512,
        }
    }
}

/// A TOTP token with full metadata.
/// Used for both local storage and DHT sync (unified struct).
#[derive(Debug, Clone, PartialEq, Eq, Reconcile, Hydrate)]
pub struct TotpTokenData {
    pub issuer: String,
    pub account_name: String,
    pub secret: String,
    pub algorithm: TotpAlgorithm,
    pub digits: u32,
    pub period: u32,
    pub created_at: u64,
    pub updated_at: u64,
}

impl TotpTokenData {
    /// Creates a new TOTP token with current timestamp.
    pub fn new(
        issuer: String,
        account_name: String,
        secret: String,
        algorithm: Algorithm,
        digits: u32,
        period: u32,
    ) -> Self {
        let now = now_epoch_secs();
        Self {
            issuer,
            account_name,
            secret,
            algorithm: algorithm.into(),
            digits,
            period,
            created_at: now,
            updated_at: now,
        }
    }

    /// Composite key for `HashMap` storage: `"{issuer}\0{account_name}"`.
    pub fn map_key(&self) -> String {
        format!("{}\0{}", self.issuer, self.account_name)
    }

    fn to_totp(&self) -> Result<Totp> {
        let secret =
            Secret::try_from_base32(&self.secret).map_err(|e| anyhow!("invalid secret: {e}"))?;
        Builder::new()
            .with_algorithm(self.algorithm.into())
            .with_digits(
                u8::try_from(self.digits).map_err(|e| anyhow!("invalid TOTP digit count: {e}"))?,
            )
            .with_secret(secret)
            .with_step_duration(u64::from(self.period))
            .build()
            .map_err(|e| anyhow!("failed to build TOTP: {e}"))
    }

    pub fn calculate_token(&self, now: u64) -> Result<String> {
        Ok(self.to_totp()?.generate(now).to_string())
    }
}

/// User-facing validation error when parsing an `otpauth://` URI.
#[derive(Debug, Error)]
pub enum TotpUriError {
    #[error("URI must start with otpauth://totp/ (HOTP is not supported).")]
    InvalidPrefix,
    #[error("{0}")]
    Totp(#[from] TotpError),
    #[error("TOTP period exceeds the supported range.")]
    PeriodOutOfRange,
    #[error("Invalid TOTP secret: {0}")]
    Secret(String),
    #[error("TOTP issuer or account name must not contain NUL bytes.")]
    ContainsNul,
}

/// Parses a TOTP QR code URI (`otpauth://totp/...`) into a validated [`TotpTokenData`].
///
/// Delegates to `totp_rs::Totp::from_url` for standards-compliant URI parsing
/// (percent-decoding, query parameter extraction, issuer resolution).
///
/// # Errors
///
/// Returns a typed [`TotpUriError`] when the URI is malformed, not an `otpauth://totp/...`
/// URI, or contains an invalid/unsupported TOTP payload (such as a secret under 128 bits).
pub fn parse_totp_uri(uri: &str) -> Result<TotpTokenData, TotpUriError> {
    if !uri.starts_with("otpauth://totp/") {
        return Err(TotpUriError::InvalidPrefix);
    }
    let totp = Totp::from_url(uri)?;
    let period = u32::try_from(totp.step()).map_err(|_| TotpUriError::PeriodOutOfRange)?;
    let issuer = totp.issuer().unwrap_or_default().to_string();
    let account_name = totp.account_name().to_string();
    if issuer.contains('\0') || account_name.contains('\0') {
        return Err(TotpUriError::ContainsNul);
    }
    let token_data = TotpTokenData::new(
        issuer,
        account_name,
        totp.secret().to_base32(),
        totp.algorithm(),
        u32::from(totp.digits()),
        period,
    );
    token_data
        .calculate_token(now_epoch_secs())
        .map_err(|e| TotpUriError::Secret(e.to_string()))?;
    Ok(token_data)
}

/// List of TOTP tokens for a user.
#[derive(Debug, Clone, Default, Reconcile, Hydrate)]
pub struct TotpTokenList {
    pub tokens: HashMap<String, TotpTokenData>,
    pub last_updated: u64,
}

impl TotpTokenList {
    /// Adds a token, returning `false` if a duplicate (same issuer + account) already exists.
    pub fn add_token(&mut self, token: TotpTokenData) -> bool {
        let key = token.map_key();
        if self.tokens.contains_key(&key) {
            return false;
        }
        self.last_updated = now_epoch_millis();
        self.tokens.insert(key, token);
        true
    }

    /// Parses a URI and adds the token to the list.
    ///
    /// Returns `false` for a duplicate, or an error if parsing/validation fails.
    pub fn add_from_uri(&mut self, uri: &str) -> Result<bool> {
        let data = parse_totp_uri(uri)?;
        Ok(self.add_token(data))
    }

    /// Removes the token by its stable map key. Returns `true` if found.
    pub fn remove_token(&mut self, key: &str) -> bool {
        if self.tokens.remove(key).is_none() {
            return false;
        }
        self.last_updated = now_epoch_millis();
        true
    }
}

const TOTP_SCHEMA_VERSION: u32 = 1;

fn pack_totp_list(list: &TotpTokenList) -> Result<Vec<u8>> {
    pack::<totp_capnp::TotpTokenList>(|mut root| {
        root.schema_version().set(TOTP_SCHEMA_VERSION);
        let len = capnp_u32(list.tokens.len());
        let mut tokens = root.tokens().init(len);
        for (i, token) in list.tokens.values().enumerate() {
            let idx = capnp_u32(i);
            let mut entry = tokens.at(idx).get();
            entry.issuer().set_str(&token.issuer);
            entry.account_name().set_str(&token.account_name);
            entry.secret().set_str(&token.secret);
            entry.algorithm().set_str(token.algorithm.as_str());
            entry.digits().set(token.digits);
            entry.period().set(token.period);
            entry.created_at().set(token.created_at);
            entry.updated_at().set(token.updated_at);
        }
        root.last_updated().set(list.last_updated);
    })
}

fn unpack_totp_list(bytes: &[u8]) -> Result<(TotpTokenList, u32)> {
    unpack::<totp_capnp::TotpTokenList, _>(bytes, |root| {
        let list = root.tokens().get();
        let mut tokens = HashMap::with_capacity(list.len() as usize);
        for i in 0..list.len() {
            let token = list.at(i);
            let data = TotpTokenData {
                issuer: token.issuer().get().as_str()?.to_string(),
                account_name: token.account_name().get().as_str()?.to_string(),
                secret: token.secret().get().as_str()?.to_string(),
                algorithm: TotpAlgorithm::from_str_lossy(token.algorithm().get().as_str()?),
                digits: token.digits(),
                period: token.period(),
                created_at: token.created_at(),
                updated_at: token.updated_at(),
            };
            tokens.insert(data.map_key(), data);
        }
        Ok((
            TotpTokenList {
                tokens,
                last_updated: root.last_updated(),
            },
            root.schema_version(),
        ))
    })
}

impl_crdt_model!(
    TotpTokenList,
    id = "totp",
    display = "TotpTokenList",
    op = OpType::Totp,
    schema = TOTP_SCHEMA_VERSION,
    pack = pack_totp_list,
    unpack = unpack_totp_list,
);
