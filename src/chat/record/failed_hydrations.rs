//! Failed hydration tracker persistence and retry flow.

use std::str;

use anyhow::Result;

use crate::{
    chat::record::hydrate_conversation,
    helpers::concurrency,
    nonfatal,
    session::epoch::SessionGuard,
    storage::{account_table::AccountTable, background_errors::report_nonfatal},
    user::ctx::UserCtx,
    veilid::core::parse_record_key,
};

/// One row per account (empty suffix) holding newline-separated
/// conversation keys that failed to hydrate.
const TABLE: AccountTable = AccountTable::new("oo_failed_hydrations");

/// Persist failed conversation keys only while this context still owns the account.
pub async fn store_failed_hydrations(
    ctx: &UserCtx,
    guard: SessionGuard,
    keys: &[String],
) -> Result<()> {
    let Some(_user_lock) = ctx.lock_for_account_commit(guard).await? else {
        return Ok(());
    };
    // A *read* error must leave the row untouched rather than rewrite it from
    // an empty default, which would erase every key recorded by earlier
    // failures and silently drop those conversations from retry.
    let mut combined = load_failed_hydrations(&ctx.account_id).await?;
    for key in keys {
        if !key.is_empty() && !combined.contains(key) {
            combined.push(key.clone());
        }
    }
    TABLE
        .store(&ctx.account_id, &[], combined.join("\n").as_bytes())
        .await
}

/// Delete all failed-hydration rows for an account (account deletion cleanup).
pub async fn delete_all_for_account(account_id: &str) -> Result<()> {
    TABLE.delete_all_for_account(account_id).await
}

/// Load the list of conversation keys that previously failed to hydrate.
async fn load_failed_hydrations(account_id: &str) -> Result<Vec<String>> {
    let bytes = TABLE.load(account_id, &[]).await?;
    let Some(bytes) = bytes else {
        return Ok(Vec::new());
    };
    // A row we cannot read is replaced rather than reported as an error the
    // caller has to fail on: the next write rebuilds it, and callers now treat
    // a read error as fatal for their own write.
    let text = match str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) => {
            report_nonfatal(
                "chat.record",
                "Corrupt failed-hydration tracker (invalid UTF-8); replacing it",
                format!("{account_id}: {error}"),
            );
            return Ok(Vec::new());
        }
    };
    Ok(text
        .lines()
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect())
}

/// Retry hydrating conversations that previously failed.
/// Called on network reconnect. Clears the list if all succeed.
pub async fn retry_failed_hydrations() {
    let Some((ctx, guard)) = UserCtx::try_current_with_guard() else {
        return;
    };
    if ctx.keypair.is_none() {
        return;
    }
    let account_id = ctx.account_id.as_ref();

    let keys = match load_failed_hydrations(account_id).await {
        Ok(keys) if keys.is_empty() => return,
        Ok(keys) => keys,
        Err(error) => {
            report_nonfatal("chat.record", "Failed to load failed hydrations", error);
            return;
        }
    };

    // Unbounded: every failed key is retried at once, so the pass costs one
    // DHT round trip instead of waves of a permit budget.
    let still_failed: Vec<String> = concurrency::map_concurrent(keys, {
        let ctx = ctx.clone();
        async move |key_str: String| {
            if !guard.is_current() {
                return None;
            }
            let Ok(manifest_key) = parse_record_key(&key_str) else {
                return None;
            };
            hydrate_conversation(manifest_key, &key_str, &ctx, guard)
                .await
                .err()
                .map(|_| key_str)
        }
    })
    .await
    .into_iter()
    .flatten()
    .collect();
    let Ok(Some(_user_lock)) = ctx.lock_for_account_commit(guard).await else {
        return;
    };
    if still_failed.is_empty() {
        nonfatal!(
            "chat.record",
            "Failed to clear hydration list",
            TABLE.delete(&ctx.account_id, &[]).await
        );
    } else {
        nonfatal!(
            "chat.record",
            "Failed to update hydration list",
            TABLE
                .store(&ctx.account_id, &[], still_failed.join("\n").as_bytes())
                .await
        );
    }
}
