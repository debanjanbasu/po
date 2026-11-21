//! Persistent app-level device identity.
//!
//! Generated once per install, stored in Veilid `TableDB`.
//! Used for multi-device routing, signal envelopes, and device management.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::{
    config::storage::DEVICE_TABLE, helpers::encryption::crypto_system, user::generate_account_id,
    veilid::table_store::Table,
};

pub const DEVICE_ID_KEY: &[u8] = b"device_id";
pub const DEVICE_LABEL_KEY: &[u8] = b"device_label";

static DEVICE_ID: Mutex<Option<Arc<str>>> = Mutex::const_new(None);
static DEVICE_LABEL: Mutex<Option<String>> = Mutex::const_new(None);

pub async fn clear_cached_device_id() {
    *DEVICE_ID.lock().await = None;
    *DEVICE_LABEL.lock().await = None;
}

/// Returns the persistent device ID, generating one on first call.
pub async fn device_id() -> Result<Arc<str>> {
    let mut cached = DEVICE_ID.lock().await;
    if let Some(id) = cached.as_ref() {
        return Ok(Arc::clone(id));
    }
    let id: Arc<str> = if let Some(bytes) = Table::open(DEVICE_TABLE, 1)
        .await?
        .load_opt(0, DEVICE_ID_KEY)
        .await?
        && let Ok(id) = str::from_utf8(&bytes)
        && id.len() == 32
        && id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Arc::from(id)
    } else {
        let id = generate_device_id().await?;
        Table::open(DEVICE_TABLE, 1)
            .await?
            .store(0, DEVICE_ID_KEY, id.as_bytes())
            .await?;
        Arc::from(id)
    };
    *cached = Some(Arc::clone(&id));
    drop(cached);
    Ok(id)
}

/// Returns the user-assigned device label, setting a platform default if absent.
pub async fn device_label() -> Result<String> {
    let mut cached = DEVICE_LABEL.lock().await;
    if let Some(label) = cached.as_ref() {
        return Ok(label.clone());
    }
    if let Some(bytes) = Table::open(DEVICE_TABLE, 1)
        .await?
        .load_opt(0, DEVICE_LABEL_KEY)
        .await?
        && let Ok(label) = str::from_utf8(&bytes)
        && !label.is_empty()
    {
        let owned = label.to_owned();
        *cached = Some(owned.clone());
        return Ok(owned);
    }
    drop(cached);
    let label = default_platform_label();
    set_device_label(label).await?;
    Ok(label.to_owned())
}

/// Persists a user-assigned device label alongside the device ID.
async fn set_device_label(label: &str) -> Result<()> {
    Table::open(DEVICE_TABLE, 1)
        .await?
        .store(0, DEVICE_LABEL_KEY, label.as_bytes())
        .await?;
    *DEVICE_LABEL.lock().await = Some(label.to_owned());
    Ok(())
}
/// Platform-based default label for new installs.
const fn default_platform_label() -> &'static str {
    #[cfg(target_os = "ios")]
    {
        "iPhone"
    }
    #[cfg(target_os = "android")]
    {
        "Android"
    }
    #[cfg(target_os = "macos")]
    {
        "Mac"
    }
    #[cfg(target_os = "windows")]
    {
        "Windows PC"
    }
    #[cfg(target_os = "linux")]
    {
        "Linux"
    }
    #[cfg(target_arch = "wasm32")]
    {
        "Browser"
    }
    #[cfg(not(any(
        target_os = "ios",
        target_os = "android",
        target_os = "macos",
        target_os = "windows",
        target_os = "linux",
        target_arch = "wasm32",
    )))]
    {
        compile_error!("default_platform_label: unsupported target — add a label for this platform")
    }
}

async fn generate_device_id() -> Result<String> {
    crypto_system!(cs);
    Ok(generate_account_id(&*cs))
}
