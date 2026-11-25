//! Cold-open hydration for a newly opened conversation: head feeds + eager roster
//! refresh, unread clearing, and session-scoped archive backfill.

use dioxus::prelude::*;
use veilid_core::{RecordKey, SharedSecret};

use crate::{
    chat::{
        conv_keys::verify_roster_binding,
        conversation,
        feed::ParticipantFeedMeta,
        record::{self, hydrate_conversation_heads, spawn_archive_backfill},
        storage,
    },
    session::epoch as session,
    storage::background_errors::report_nonfatal,
    user::ctx::UserCtx,
    veilid::{core::parse_record_key, store::report_nonfatal_unless_network},
};

use super::{ConversationLoadState, key_is_current};

#[derive(Clone, Copy)]
struct OpenSignals {
    current_key: Signal<String>,
    load_state: Signal<ConversationLoadState>,
    guard: session::SessionGuard,
}

impl OpenSignals {
    fn is_current(&self, ck: &str) -> bool {
        key_is_current(&self.current_key, ck, self.guard)
    }
}

fn has_archive_backfill(feed_metas: &[Option<ParticipantFeedMeta>]) -> bool {
    feed_metas
        .iter()
        .flatten()
        .any(|feed_meta| !feed_meta.archive_root_key.is_empty())
}

fn spawn_cold_open_backfill(
    conversation_key: String,
    ctx: UserCtx,
    feed_metas: Vec<Option<ParticipantFeedMeta>>,
    secret: SharedSecret,
    mut signals: OpenSignals,
) {
    spawn_archive_backfill(
        signals.guard,
        ctx,
        conversation_key,
        feed_metas,
        secret,
        move |result, conversation_key| {
            if !signals.is_current(conversation_key) {
                return;
            }
            match result {
                Ok(_) => signals.load_state.with_mut(|state| {
                    state.loading_head = false;
                    state.loading_history = false;
                    state.error = None;
                }),
                Err(error) => {
                    report_nonfatal(
                        "chat.hooks",
                        "Failed to backfill conversation archives",
                        format!("{conversation_key}: {error}"),
                    );
                    signals.load_state.set(ConversationLoadState {
                        loading_head: false,
                        loading_history: false,
                        error: Some(
                            "Older messages could not be loaded right now. New messages will still sync."
                                .to_string(),
                        ),
                    });
                }
            }
        },
    );
}

async fn refresh_local_roster(
    ck: &str,
    ctx: &UserCtx,
    manifest_key: &RecordKey,
    conv: &storage::LocalConversation,
    mut signals: OpenSignals,
) {
    let secret = conv.conv_secret();
    // A refreshed roster is only saved once its record's writer slots back it;
    // see `verify_roster_binding`.
    let refreshed = match record::load_roster(manifest_key.clone(), &secret).await {
        Ok(roster) => verify_roster_binding(manifest_key, &roster)
            .await
            .map(|()| roster),
        Err(error) => Err(error),
    };
    match refreshed {
        Ok(roster) => {
            if !signals.is_current(ck) {
                return;
            }
            let _user_lock = match ctx.lock_for_account_commit(signals.guard).await {
                Ok(Some(lock)) => lock,
                Ok(None) => return,
                Err(error) => {
                    report_nonfatal(
                        "chat.hooks",
                        "Failed to verify account before saving refreshed roster",
                        error,
                    );
                    return;
                }
            };
            if let Err(error) = storage::mutate_for_account(&ctx.account_id, ck, |conv| {
                if !signals.guard.is_current() || !conv.has_conv_key() || conv.roster == roster {
                    return false;
                }
                conv.roster = roster;
                true
            })
            .await
            {
                report_nonfatal("chat.hooks", "Failed to save refreshed roster", error);
                if signals.is_current(ck) {
                    signals.load_state.with_mut(|state| {
                        state.error = Some(
                            "Conversation updates could not be saved. Please retry.".to_string(),
                        );
                    });
                }
            }
        }
        Err(e) => {
            // Transient DHT reads (offline, cold routing table) are normal
            // enough that the global network indicator already says why; only
            // a real failure belongs in the error surface. The in-UI copy
            // below stays either way — saved messages are still available.
            report_nonfatal_unless_network(
                "chat.cold_open",
                "Failed to refresh conversation roster",
                &e,
            );
            if signals.is_current(ck) {
                signals.load_state.with_mut(|state| {
                    state.error = Some(
                        "Could not refresh this conversation. Saved messages remain available."
                            .to_string(),
                    );
                });
            }
        }
    }
}

async fn hydrate_cold_open_conversation(
    ck: &str,
    ctx: &UserCtx,
    manifest_key: RecordKey,
    mut signals: OpenSignals,
) {
    if !signals.is_current(ck) {
        return;
    }
    signals.load_state.set(ConversationLoadState {
        loading_head: true,
        loading_history: false,
        error: None,
    });
    match hydrate_conversation_heads(&manifest_key, ck, ctx, signals.guard).await {
        Ok(hydrated) => {
            if !signals.is_current(ck) {
                return;
            }
            let history_loading = has_archive_backfill(&hydrated.feed_metas);
            signals.load_state.set(ConversationLoadState {
                loading_head: false,
                loading_history: history_loading,
                error: None,
            });
            if history_loading {
                spawn_cold_open_backfill(
                    ck.to_string(),
                    ctx.clone(),
                    hydrated.feed_metas,
                    hydrated.conv.conv_secret(),
                    signals,
                );
            }
        }
        Err(e) => {
            // Same policy as the roster refresh above: an offline read is the
            // network indicator's job, not a conversation error. The key stays
            // in the message and the error keeps its type, so
            // `is_transient_network_error` can still see the marker; the
            // in-UI copy below names what to do either way.
            report_nonfatal_unless_network(
                "chat.hooks",
                "Cold-start hydration failed",
                &e.context(format!("conversation key {ck}")),
            );
            if !signals.is_current(ck) {
                return;
            }
            signals.load_state.set(ConversationLoadState {
                loading_head: false,
                loading_history: false,
                error: Some(
                    "Conversation could not be loaded. Please reopen it and try again.".to_string(),
                ),
            });
        }
    }
}

/// Network recovery only persists local data; the reactive projection owns
/// displayed messages and peer keys independently of this work.
pub(super) async fn recover_conversation(
    key_memo: Memo<String>,
    identity_memo: Memo<Option<UserCtx>>,
    current_key: Signal<String>,
    load_state: Signal<ConversationLoadState>,
) {
    let ck = key_memo();
    let Some(identity_snapshot) = identity_memo() else {
        return;
    };
    let Some((identity, guard)) = UserCtx::try_current_with_guard() else {
        return;
    };
    if identity.account_id != identity_snapshot.account_id {
        return;
    }
    let mut signals = OpenSignals {
        current_key,
        load_state,
        guard,
    };
    if !signals.is_current(&ck) {
        return;
    }
    signals.load_state.set(ConversationLoadState::default());

    let loaded_conv = match storage::load_for_account(&identity.account_id, &ck).await {
        Ok(conv) => Some(conv),
        Err(e) => {
            report_nonfatal("chat.hooks", "Failed to load initial feed messages", e);
            return;
        }
    };

    if identity.keypair.is_none() || !signals.is_current(&ck) {
        return;
    }
    let Ok(manifest_key) = parse_record_key(&ck) else {
        return;
    };

    match loaded_conv {
        Some(conv) if conv.has_conv_key() => {
            refresh_local_roster(&ck, &identity, &manifest_key, &conv, signals).await;
        }
        _ => {
            hydrate_cold_open_conversation(&ck, &identity, manifest_key, signals).await;
        }
    }

    if !signals.is_current(&ck) {
        return;
    }
    session::spawn_session_scoped(async move {
        conversation::mutate_entry(&ck, |entry| {
            if entry.unread_count == 0 {
                return false;
            }
            entry.unread_count = 0;
            true
        })
        .await;
    });
}
