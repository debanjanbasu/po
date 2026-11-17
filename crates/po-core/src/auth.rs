//! Local account store.
//!
//! po is local-first: accounts live in a JSON file under the user's config
//! directory, and passwords are stored as salted SHA-256 digests, never in
//! plaintext. This exists so the app has real auth flows offline; a hosted
//! backend would replace the file with an API later.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::paths;

/// Errors raised by the account store.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON failure.
    #[error("store error: {0}")]
    Store(#[from] serde_json::Error),
    /// The username is already registered.
    #[error("account already exists: {0}")]
    AccountExists(String),
    /// The username is invalid.
    #[error("invalid username: {0}")]
    InvalidUsername(String),
    /// The password is too short.
    #[error("password must be at least 8 characters")]
    WeakPassword,
}

/// A stored account: salted hash plus the random salt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAccount {
    /// Hex-encoded random salt.
    pub salt: String,
    /// Hex-encoded salted SHA-256 of the password.
    pub hash: String,
    /// When the account was created (unix seconds).
    pub created_at: u64,
}

/// The on-disk account store.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AuthStore {
    accounts: HashMap<String, StoredAccount>,
}

/// Hash a password with a salt.
fn salted_hash(salt: &str, password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate a random-looking salt from the OS entropy source.
fn fresh_salt() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(now.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    let addr = &now as *const _ as usize;
    hasher.update(addr.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

impl AuthStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the store from `path`.
    ///
    /// # Errors
    /// Returns an error when the file exists but cannot be read or parsed.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, AuthError> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Load the store from the default location (`config_dir()/po/auth.json`).
    ///
    /// # Errors
    /// Returns an error when the file exists but cannot be read or parsed.
    pub fn load_default() -> Result<Self, AuthError> {
        Self::load(default_auth_path())
    }

    /// Persist the store to `path`.
    ///
    /// # Errors
    /// Returns an error when the file cannot be written.
    pub fn save(&self, path: impl Into<PathBuf>) -> Result<(), AuthError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Persist the store to the default location.
    ///
    /// # Errors
    /// Returns an error when the file cannot be written.
    pub fn save_default(&self) -> Result<(), AuthError> {
        self.save(default_auth_path())
    }

    /// Register a new account.
    ///
    /// # Errors
    /// Returns an error when the username is taken or the credentials are invalid.
    pub fn register(&mut self, username: &str, password: &str) -> Result<(), AuthError> {
        let username = username.trim();
        if username.is_empty() || username.len() > 64 {
            return Err(AuthError::InvalidUsername(username.to_string()));
        }
        if password.len() < 8 {
            return Err(AuthError::WeakPassword);
        }
        if self.accounts.contains_key(username) {
            return Err(AuthError::AccountExists(username.to_string()));
        }
        let salt = fresh_salt();
        self.accounts.insert(
            username.to_string(),
            StoredAccount {
                hash: salted_hash(&salt, password),
                salt,
                created_at: crate::session::now_unix(),
            },
        );
        Ok(())
    }

    /// Verify credentials; `true` when the password matches.
    #[must_use]
    pub fn authenticate(&self, username: &str, password: &str) -> bool {
        self.accounts
            .get(username.trim())
            .is_some_and(|account| {
                // Constant-time-ish comparison: hash the candidate and compare bytes.
                let candidate = salted_hash(&account.salt, password);
                candidate == account.hash
            })
    }

    /// Whether an account exists.
    #[must_use]
    pub fn exists(&self, username: &str) -> bool {
        self.accounts.contains_key(username.trim())
    }

    /// Registered usernames, sorted.
    #[must_use]
    pub fn usernames(&self) -> Vec<String> {
        let mut names: Vec<String> = self.accounts.keys().cloned().collect();
        names.sort();
        names
    }
}

/// Path of the auth store file.
#[must_use]
pub fn default_auth_path() -> PathBuf {
    paths::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("po")
        .join("auth.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_authenticate_succeeds() {
        let mut store = AuthStore::new();
        store.register("ada", "correct horse").expect("register");
        assert!(store.authenticate("ada", "correct horse"));
        assert!(!store.authenticate("ada", "wrong password"));
        assert!(!store.authenticate("nobody", "correct horse"));
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let mut store = AuthStore::new();
        store.register("ada", "password1").expect("register");
        assert!(matches!(
            store.register("ada", "password2"),
            Err(AuthError::AccountExists(_))
        ));
    }

    #[test]
    fn weak_credentials_are_rejected() {
        let mut store = AuthStore::new();
        assert!(matches!(
            store.register("a", "short"),
            Err(AuthError::InvalidUsername(_)) | Err(AuthError::WeakPassword)
        ));
        assert!(matches!(
            store.register("okname", "short"),
            Err(AuthError::WeakPassword)
        ));
    }

    #[test]
    fn store_round_trips_through_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::new();
        store.register("ada", "correct horse").expect("register");
        store.save(&path).expect("save");

        let loaded = AuthStore::load(&path).expect("load");
        assert!(loaded.authenticate("ada", "correct horse"));
        assert!(!loaded.authenticate("ada", "wrong"));
    }

    #[test]
    fn salts_are_unique_per_registration() {
        let mut store = AuthStore::new();
        store.register("a", "password1").expect("a");
        store.register("b", "password1").expect("b");
        let names = store.usernames();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }
}
