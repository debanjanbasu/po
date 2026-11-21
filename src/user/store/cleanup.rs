//! Account-deletion cascade: durable cleanup markers and user-scoped row removal.
use crate::{
    chat::{
        contact::ContactList, conversation::ConversationIndex, record::delete_all_for_account,
        storage as chat_storage,
    },
    compression::training_queue,
    config::storage::DEVICE_TABLE,
    helpers::store_lock::{lock_store, recover_poison},
    models::{app_state::AppState, totp::TotpTokenList},
    platform::device_id::{DEVICE_ID_KEY, DEVICE_LABEL_KEY, clear_cached_device_id},
    storage::{
        background_errors::report_nonfatal,
        snapshots,
        store_trait::{CrdtModel, DUAL_FORMAT_TABLE_COLS, delete_row, notify_model_changed},
    },
    sync::pending_ops,
    user::{identity_doc, load_user},
    veilid::table_store::Table,
};
use anyhow::{Result, anyhow};
use std::{
    fmt::Display,
    future::Future,
    mem,
    sync::{LazyLock, Mutex},
};

use super::{
    COL_PASSWORD_HASH, COL_PASSWORD_ROTATION, COL_USER, DEVICE_KEY_KEY, DEVICE_KEY_KIND_KEY,
    DEVICE_WRAP_KEY_LOCK_KEY, LAST_ACTIVE_KEY, PAIRING_ATTEMPTS_PREFIX, PIN_ATTEMPTS_PREFIX,
    SESSION_COL, clear_cached_device_wrap_key, pin_key, session_table, users_table,
};

const STORE_CTX: &str = "models.user.store";
/// Named by the recompression scan (`compression::recompression`), which must
/// re-derive these rows when a dictionary swaps, so it is `pub` rather than a
/// string literal there that would silently drift on a rename.
pub const CLEANUP_TABLE: &str = "oo_cleanup_incomplete";
const CLEANUP_LOCK_KEY: &[u8] = b"account-cleanup";
static CLEANUP_FAILURES: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub async fn is_cleanup_pending(account_id: &str) -> Result<bool> {
    Ok(Table::open(CLEANUP_TABLE, 1)
        .await?
        .load_opt(0, account_id.as_bytes())
        .await?
        .is_some())
}

fn report_cleanup_error(message: &'static str, details: String) {
    recover_poison(CLEANUP_FAILURES.lock()).push(format!("{message}: {details}"));
    report_nonfatal(STORE_CTX, message, details);
}

async fn delete_table_row_nonfatal(
    table: &Table,
    col: u32,
    key: &[u8],
    message: &'static str,
    details: impl FnOnce() -> String,
) {
    if let Err(e) = table.delete(col, key).await {
        report_cleanup_error(message, format!("{} error={e}", details()));
    }
}

async fn delete_crdt_model_nonfatal<T: CrdtModel>(key: &[u8], account_id: &str) {
    match Table::open(T::TABLE_NAME, DUAL_FORMAT_TABLE_COLS).await {
        Ok(table) => {
            // Row, doc, and every reserved metadata entry (currentness marker,
            // compaction watermark) in one batch; a surviving entry would
            // outlive the account's data.
            if let Err(e) = delete_row(&table, key).await {
                report_cleanup_error(
                    "Failed to delete account-scoped row during account cleanup",
                    format!("table={} account_id={account_id} error={e}", T::TABLE_NAME),
                );
            }
        }
        Err(e) => report_cleanup_error(
            "Failed to open CRDT model table during account cleanup",
            format!("table={} account_id={account_id} error={e}", T::TABLE_NAME),
        ),
    }
    notify_model_changed::<T>();
}

async fn cleanup_username_tables(username: &str, ukey: &[u8]) {
    match users_table().await {
        Ok(table) => {
            delete_table_row_nonfatal(
                &table,
                COL_USER,
                ukey,
                "Failed to delete user row during account cleanup",
                || format!("username={username}"),
            )
            .await;
            delete_table_row_nonfatal(
                &table,
                COL_PASSWORD_HASH,
                ukey,
                "Failed to delete password-hash row during account cleanup",
                || format!("username={username}"),
            )
            .await;
        }
        Err(e) => report_cleanup_error(
            "Failed to open users table during account cleanup",
            format!("username={username} error={e}"),
        ),
    }

    match session_table().await {
        Ok(table) => {
            match table.load_opt(SESSION_COL, LAST_ACTIVE_KEY).await {
                Ok(Some(active_user)) if active_user == ukey => {
                    delete_table_row_nonfatal(
                        &table,
                        SESSION_COL,
                        LAST_ACTIVE_KEY,
                        "Failed to delete last-active row during account cleanup",
                        || format!("username={username}"),
                    )
                    .await;
                }
                Ok(_) => {}
                Err(e) => report_cleanup_error(
                    "Failed to read last-active row during account cleanup",
                    format!("username={username} error={e}"),
                ),
            }
            let pin_attempt_key = pin_key(username);
            for key in table
                .raw()
                .get_keys(SESSION_COL)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|key| key.starts_with(&pin_attempt_key))
            {
                delete_table_row_nonfatal(
                    &table,
                    SESSION_COL,
                    &key,
                    "Failed to delete pin-attempt row during account cleanup",
                    || format!("username={username}"),
                )
                .await;
            }
        }
        Err(e) => report_cleanup_error(
            "Failed to open session table during account cleanup",
            format!("username={username} error={e}"),
        ),
    }
}

/// Run an async cleanup operation, reporting non-fatal errors.
async fn cleanup_nonfatal(
    message: &'static str,
    details: impl Display,
    fut: impl Future<Output = Result<()>>,
) {
    if let Err(e) = fut.await {
        report_cleanup_error(message, format!("{details} error={e}"));
    }
}

async fn cleanup_account_tables(account_id: &str, akey: &[u8]) {
    match users_table().await {
        Ok(table) => {
            delete_table_row_nonfatal(
                &table,
                COL_PASSWORD_ROTATION,
                akey,
                "Failed to delete password-rotation history",
                || format!("account_id={account_id}"),
            )
            .await;
        }
        Err(error) => report_cleanup_error(
            "Failed to open password-rotation history during cleanup",
            format!("account_id={account_id} error={error}"),
        ),
    }
    delete_crdt_model_nonfatal::<AppState>(akey, account_id).await;
    delete_crdt_model_nonfatal::<TotpTokenList>(akey, account_id).await;
    delete_crdt_model_nonfatal::<ContactList>(akey, account_id).await;
    delete_crdt_model_nonfatal::<ConversationIndex>(akey, account_id).await;

    cleanup_nonfatal(
        "Failed to delete snapshot rows during account cleanup",
        format_args!("account_id={account_id}"),
        snapshots::delete_all_for_account(akey),
    )
    .await;
    cleanup_nonfatal(
        "Failed to delete identity docs during account cleanup",
        format_args!("account_id={account_id}"),
        identity_doc::delete_docs(account_id),
    )
    .await;
    cleanup_nonfatal(
        "Failed to delete local conversations during account cleanup",
        format_args!("account_id={account_id}"),
        chat_storage::delete_all_for_account(account_id),
    )
    .await;
    cleanup_nonfatal(
        "Failed to delete pending invites during account cleanup",
        format_args!("account_id={account_id}"),
        chat_storage::delete_all_pending_invites_for_account(account_id),
    )
    .await;
    cleanup_nonfatal(
        "Failed to delete failed-hydration rows during account cleanup",
        format_args!("account_id={account_id}"),
        delete_all_for_account(account_id),
    )
    .await;
    cleanup_nonfatal(
        "Failed to delete training samples during account cleanup",
        format_args!("account_id={account_id}"),
        training_queue::delete_all_for_account(account_id),
    )
    .await;
}

async fn cleanup_device_rows_if_no_users_remain() {
    let table = match users_table().await {
        Ok(table) => table,
        Err(e) => {
            report_cleanup_error(
                "Failed to open users table while checking remaining accounts",
                format!("error={e}"),
            );
            return;
        }
    };
    match table.raw().get_keys(COL_USER).await {
        Ok(keys) if !keys.is_empty() => return,
        Ok(_) => {}
        Err(e) => {
            report_cleanup_error(
                "Failed to inspect remaining users during account cleanup",
                format!("error={e}"),
            );
            return;
        }
    }
    let device_table = match Table::open(DEVICE_TABLE, 1).await {
        Ok(device_table) => device_table,
        Err(e) => {
            report_cleanup_error(
                "Failed to open device table during final account cleanup",
                format!("error={e}"),
            );
            return;
        }
    };
    let _guard = lock_store(DEVICE_WRAP_KEY_LOCK_KEY).await;
    // Re-check under the lock: a concurrent signup or import may have created
    // a user between the unlocked check above and lock acquisition.
    match table.raw().get_keys(COL_USER).await {
        Ok(keys) if !keys.is_empty() => return,
        Ok(_) => {}
        Err(e) => {
            report_cleanup_error(
                "Failed to re-check remaining users under device lock",
                format!("error={e}"),
            );
            return;
        }
    }
    for key in [
        DEVICE_KEY_KEY,
        DEVICE_KEY_KIND_KEY,
        DEVICE_ID_KEY,
        DEVICE_LABEL_KEY,
    ] {
        delete_table_row_nonfatal(
            &device_table,
            0,
            key,
            "Failed to delete device row during final account cleanup",
            || format!("key={}", String::from_utf8_lossy(key)),
        )
        .await;
    }
    clear_cached_device_wrap_key().await;
    clear_cached_device_id().await;
    let session = match session_table().await {
        Ok(session) => session,
        Err(e) => {
            report_cleanup_error(
                "Failed to open session table during final account cleanup",
                format!("error={e}"),
            );
            return;
        }
    };
    match session.raw().get_keys(SESSION_COL).await {
        Ok(keys) => {
            for key in keys {
                // Both attempt families live in this column and neither is
                // username-scoped by prefix alone: pin rows carry the username,
                // pairing rows a payload-nonce hash. The device is being wiped,
                // so every row of both goes.
                if key.starts_with(PIN_ATTEMPTS_PREFIX) || key.starts_with(PAIRING_ATTEMPTS_PREFIX)
                {
                    delete_table_row_nonfatal(
                        &session,
                        SESSION_COL,
                        &key,
                        "Failed to delete attempt row during final account cleanup",
                        || format!("key={}", String::from_utf8_lossy(&key)),
                    )
                    .await;
                }
            }
        }
        Err(e) => report_cleanup_error(
            "Failed to list session keys during final account cleanup",
            format!("error={e}"),
        ),
    }
}

/// Deletes ALL local data for a user across every table store table.
///
/// Tables cleared:
/// - `oo_users` (user blob + password hash) — username-keyed; rotation history is account-keyed
/// - `oo_session` (last active + PIN attempts) — username-keyed
/// - `oo_pending_ops` (pending + dead-letter columns) — exact account-ID cleanup of queued sync work
/// - `oo_snapshots` — account-ID keyed archived CRDT snapshots
/// - `oo_app_state` (nav order, 2-col) — account-ID-keyed
/// - `oo_{id}` for each `CrdtModel` (2-col: snapshot + automerge) — account-ID-keyed
/// - `oo_conversations` — prefix-scoped, delegated to `chat::storage`
/// - `oo_pending_invites`, `oo_failed_hydrations` — account-prefix-scoped
/// - `oo_learned_samples` — prefix-scoped, delegated to `compression::training_queue`
/// - `oo_device` — cleared only when the deleted account was the final local account on the device
///
/// Every cleanup step is attempted. If any step fails, a durable account-ID-keyed
/// marker is retained so startup can retry the full idempotent cleanup.
pub async fn delete_all_user_data(username: &str, account_id: &str) -> Result<()> {
    let _guard = lock_store(CLEANUP_LOCK_KEY).await;
    let _user_guard = lock_store(username.as_bytes()).await;
    let _account_guard = lock_store(account_id.as_bytes()).await;
    recover_poison(CLEANUP_FAILURES.lock()).clear();
    let ukey = username.as_bytes();
    let akey = account_id.as_bytes();
    let markers = Table::open(CLEANUP_TABLE, 1).await?;
    markers.store(0, akey, username.as_bytes()).await?;
    match load_user(username).await {
        Ok(user)
            if user
                .as_ref()
                .is_none_or(|user| user.account_id() == account_id) =>
        {
            cleanup_username_tables(username, ukey).await;
        }
        Ok(_) => {}
        Err(error) => report_cleanup_error(
            "Could not verify username ownership during account cleanup",
            format!("username={username} account_id={account_id} error={error}"),
        ),
    }
    if let Err(e) = pending_ops::delete_all_for_account(username, account_id).await {
        report_cleanup_error(
            "Failed to delete pending-queue rows during account cleanup",
            format!("username={username} error={e}"),
        );
    }
    cleanup_account_tables(account_id, akey).await;
    cleanup_device_rows_if_no_users_remain().await;
    let failures = mem::take(&mut *recover_poison(CLEANUP_FAILURES.lock()));
    if failures.is_empty() {
        markers.delete(0, akey).await?;
        Ok(())
    } else {
        Err(anyhow!(
            "account cleanup incomplete: {}",
            failures.join("; ")
        ))
    }
}

/// Retries all cleanup cascades that previously completed only partially.
pub async fn retry_incomplete_cleanups() -> Result<()> {
    let table = Table::open(CLEANUP_TABLE, 1).await?;
    for account_id in table.raw().get_keys(0).await? {
        let Some(username) = table.load_opt(0, &account_id).await? else {
            continue;
        };
        let account_id = String::from_utf8(account_id.into())?;
        let username = String::from_utf8(username.to_vec())?;
        if let Err(e) = delete_all_user_data(&username, &account_id).await {
            report_nonfatal(STORE_CTX, "Automatic account cleanup retry failed", e);
        }
    }
    Ok(())
}
