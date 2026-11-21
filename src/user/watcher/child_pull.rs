//! Child-record pulls (TOTP, contacts, conversations): single-feature and
//! joined full-pull variants, with change notification and stale-copy
//! re-publication.
use anyhow::Result;
use tokio::join;
use veilid_core::{KeyPair, RecordKey, SharedSecret};

use crate::{
    chat::{
        contact::ContactList, conversation::ConversationIndex, record::sweep_advanced_conversations,
    },
    helpers::spawn,
    models::{pending_operation::OpType, totp::TotpTokenList},
    nonfatal,
    session::epoch as session,
    storage::{
        background_errors::{push_background_error, report_nonfatal},
        store_trait::{CrdtModel, PullOutcome, load_local, pull_and_merge},
        sync_status::refresh_sync_status,
    },
    user::{
        ctx::UserCtx,
        identity_context::{apply_schema_fallback, is_merge_schema_error},
    },
    veilid::store,
};

use super::shared::{current_identity, prepare_pull};

/// Pulls one child feature's model from the DHT and merges it into local
/// state, refreshing sync status when anything changed.
/// Returns whether the pull produced local changes.
pub(super) async fn pull_child_for_feature(feature: &str) -> bool {
    let guard = session::SessionGuard::capture();
    let Some((user_key, ctx)) = current_identity() else {
        return false;
    };
    let Some((identity, _crypto_lease)) = prepare_pull(&ctx, guard).await else {
        return false;
    };

    // Snapshot the index so a landing conversations pull can sweep only the
    // conversations whose `last_message_at` actually advanced.
    let index_before = if feature == "conversations" {
        load_local::<ConversationIndex>(ctx.account_id.as_bytes())
            .await
            .ok()
    } else {
        None
    };

    let outcome = match feature {
        "totp" => {
            pull_child::<TotpTokenList>(
                &ctx,
                &user_key,
                &identity.secret,
                &identity.keypair,
                &guard,
                "authenticator entries",
            )
            .await
        }
        "contacts" => {
            pull_child::<ContactList>(
                &ctx,
                &user_key,
                &identity.secret,
                &identity.keypair,
                &guard,
                "contacts",
            )
            .await
        }
        "conversations" => {
            pull_child::<ConversationIndex>(
                &ctx,
                &user_key,
                &identity.secret,
                &identity.keypair,
                &guard,
                "conversation index",
            )
            .await
        }
        _ => return false,
    };

    // A nudge/poll names the index, but the message that moved it lives in a
    // conversation record whose own `ValueChange` may have been lost; sweep
    // exactly the entries the index pull showed advancing.
    if feature == "conversations"
        && outcome.changed
        && guard.is_current()
        && let Ok(index_after) = load_local::<ConversationIndex>(ctx.account_id.as_bytes()).await
    {
        sweep_advanced_conversations(&ctx, index_before.as_ref(), &index_after, guard).await;
    }

    if outcome.changed && guard.is_current() {
        refresh_sync_status().await;
    }
    outcome.changed
}

/// Pulls one child model, funnels errors, notifies the UI on change, and
/// re-publishes local state over a stale network copy.
async fn republish_stale_ops(ctx: &UserCtx, guard: session::SessionGuard, ops: &[OpType]) {
    if ops.is_empty() || !guard.is_current() {
        return;
    }
    match ctx.lock_for_account_commit(guard).await {
        Ok(Some(_user_lock)) => {
            for &op in ops {
                if !guard.is_current() {
                    break;
                }
                nonfatal!(
                    "identity.watcher",
                    "Failed to re-publish child record over stale DHT copy",
                    ctx.enqueue_touch(op).await
                );
            }
        }
        Ok(None) => {}
        Err(error) => report_nonfatal(
            "identity.watcher",
            "Failed to verify account before re-publishing stale child records",
            error,
        ),
    }
}

/// Pulls one child model, funnels errors, notifies the UI on change, and
/// re-publishes local state over a stale network copy.
async fn pull_child<T: CrdtModel>(
    ctx: &UserCtx,
    user_key: &RecordKey,
    secret: &SharedSecret,
    keypair: &KeyPair,
    guard: &session::SessionGuard,
    what: &str,
) -> PullOutcome {
    let outcome = child_pull_outcome::<T>(
        pull_and_merge::<T>(ctx, *guard, user_key, secret, keypair).await,
        what,
        ctx,
        *guard,
    )
    .await;
    if outcome.remote_stale {
        republish_stale_ops(ctx, *guard, &[T::OP_TYPE]).await;
    }
    outcome
}

pub(in crate::user) async fn pull_identity_children(
    ctx: &UserCtx,
    user_key: &RecordKey,
    secret: &SharedSecret,
    keypair: &KeyPair,
    guard: session::SessionGuard,
) -> bool {
    let (totp_res, chat_res, contacts_res) = join!(
        pull_and_merge::<TotpTokenList>(ctx, guard, user_key, secret, keypair),
        pull_and_merge::<ConversationIndex>(ctx, guard, user_key, secret, keypair),
        pull_and_merge::<ContactList>(ctx, guard, user_key, secret, keypair),
    );
    let totp =
        child_pull_outcome::<TotpTokenList>(totp_res, "authenticator entries", ctx, guard).await;
    let conversations =
        child_pull_outcome::<ConversationIndex>(chat_res, "conversation index", ctx, guard).await;
    let contacts = child_pull_outcome::<ContactList>(contacts_res, "contacts", ctx, guard).await;

    if guard.is_current() && conversations.changed {
        // Reconcile session watches after the index gains or loses entries.
        spawn(async move {
            crate::chat::record::arm_and_catch_up(false).await;
        });
    }

    let stale_ops: Vec<OpType> = [
        (totp.remote_stale, TotpTokenList::OP_TYPE),
        (conversations.remote_stale, ConversationIndex::OP_TYPE),
        (contacts.remote_stale, ContactList::OP_TYPE),
    ]
    .into_iter()
    .filter_map(|(stale, op)| stale.then_some(op))
    .collect();
    republish_stale_ops(ctx, guard, &stale_ops).await;

    totp.changed || conversations.changed || contacts.changed
}

/// Unwrap a child pull result, reporting failures through the standard
/// funnel and treating them as "nothing changed, nothing stale".
pub(in crate::user) async fn child_pull_outcome<T: CrdtModel>(
    result: Result<PullOutcome>,
    what: &str,
    ctx: &UserCtx,
    guard: session::SessionGuard,
) -> PullOutcome {
    match result {
        Ok(outcome) => outcome,
        Err(error) => {
            if !guard.is_current() {
                return PullOutcome {
                    changed: false,
                    remote_stale: false,
                };
            }
            // Schema-corruption fallback policy, applied consistently on
            // pull and publish: a stale child record clears its sync pointer.
            apply_schema_fallback::<T>(
                &ctx.account_id,
                Some(&ctx.username),
                Some(guard),
                is_merge_schema_error(&error),
            )
            .await;
            if !guard.is_current() {
                return PullOutcome {
                    changed: false,
                    remote_stale: false,
                };
            }
            let error = error.context(T::DISPLAY_NAME);
            if store::report_nonfatal_unless_network(
                "identity.watcher",
                "Failed to pull child record",
                &error,
            ) {
                push_background_error(
                    "identity.watcher",
                    format!("Failed to refresh {what} from network."),
                );
            }
            PullOutcome {
                changed: false,
                remote_stale: false,
            }
        }
    }
}
