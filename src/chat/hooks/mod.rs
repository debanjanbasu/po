//! Chat store hook: loads messages from participant feeds, watches for remote
//! changes, and provides send/typing actions with session epoch guards.

mod cold_open;
mod send;

pub use send::send_from_deep_link;

use std::sync::Arc;

use dioxus::prelude::*;

use crate::{
    chat::{
        AccountScoped,
        conversation::{self, TYPING_GENERATION, typing_at_for, typing_version_for},
        current_snapshot,
        feed::FeedMessage,
        manager::{clear_open_conversation, ensure_watched_if_new, set_open_conversation},
        record::sweep_rearmed_conversation,
        signal::{self, PeerSignal, SignalPayload, signal_all_peers},
        storage,
    },
    config,
    global_state::conversation_meta,
    helpers::async_utils::{now_epoch_millis, sleep_ms},
    session::epoch as session,
    storage::background_errors::report_nonfatal,
    user::ctx::UserCtx,
};

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ConversationLoadState {
    pub loading_head: bool,
    pub loading_history: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy)]
pub struct ChatStore {
    pub messages: Memo<Arc<Vec<FeedMessage>>>,
    pub is_typing: Memo<bool>,
    pub send_error: Signal<Option<String>>,
    pub load_state: Signal<ConversationLoadState>,
    pub local_loading: Memo<bool>,
    pub local_error: Memo<Option<String>>,
    pub is_sending: Signal<bool>,
    pub retry_load: Callback<()>,
    pub send_message: Callback<String>,
    pub send_typing: Callback<()>,
}

pub(super) fn key_is_current(
    current_key: &Signal<String>,
    ck: &str,
    guard: session::SessionGuard,
) -> bool {
    guard.is_current() && (*current_key).peek().as_str() == ck
}

/// Captured session guard + conversation-key signal for guarding UI signal
/// writes across awaits in the send flow. Mutations are skipped when the
/// session changed or the user navigated to another conversation.
#[derive(Clone, Copy)]
pub(super) struct SendGuard {
    pub(super) current_key: Signal<String>,
    pub(super) session: session::SessionGuard,
}

impl SendGuard {
    pub(super) fn live(&self, ck: &str) -> bool {
        key_is_current(&self.current_key, ck, self.session)
    }
}

struct LocalSnapshot {
    account_id: Arc<str>,
    key: String,
    conversation: Result<Arc<storage::LocalConversation>, String>,
    messages: Arc<Vec<FeedMessage>>,
}

impl AccountScoped for LocalSnapshot {
    fn account_id(&self) -> &Arc<str> {
        &self.account_id
    }
}

fn use_local_snapshot(
    key_memo: Memo<String>,
    identity_memo: Memo<Option<UserCtx>>,
) -> Resource<Option<LocalSnapshot>> {
    let meta_version = use_memo(move || {
        conversation_meta()
            .read()
            .get(key_memo().as_str())
            .map(|snap| snap.last_feed_update)
    });

    use_resource(move || {
        meta_version();
        let key = key_memo();
        let identity = identity_memo();
        async move {
            let identity = identity?;
            let (conversation, messages) = match storage::load_for_account(
                &identity.account_id,
                &key,
            )
            .await
            {
                Ok(conv) => {
                    let messages = Arc::clone(&conv.messages);
                    (Ok(conv), messages)
                }
                Err(e) => {
                    report_nonfatal("chat.hooks", "Failed to load saved conversation", e);
                    (
                        Err("Saved messages could not be loaded. Please try again.".to_string()),
                        Arc::new(Vec::new()),
                    )
                }
            };
            Some(LocalSnapshot {
                account_id: identity.account_id,
                key,
                conversation,
                messages,
            })
        }
    })
}

fn use_send_message_action(
    key_memo: Memo<String>,
    identity_memo: Memo<Option<UserCtx>>,
    current_key: Signal<String>,
    mut send_error: Signal<Option<String>>,
    mut is_sending: Signal<bool>,
    on_saved: Callback<Arc<str>>,
) -> Callback<String> {
    Callback::new(move |text: String| {
        let trimmed = text.trim();
        if trimmed.is_empty() || *is_sending.peek() {
            return;
        }
        let text: Arc<str> = Arc::from(trimmed);

        let ck = key_memo();
        let identity = identity_memo();
        let guard = SendGuard {
            current_key,
            session: session::SessionGuard::capture(),
        };
        send_error.set(None);
        is_sending.set(true);

        session::spawn_session_scoped(async move {
            send::do_send_message(&ck, identity, guard, on_saved, &mut send_error, text).await;
            if guard.live(&ck) {
                is_sending.set(false);
            }
        });
    })
}

fn send_typing_signal(ck: &str, peers: &[String], typing: bool) {
    let Some((conv_key, sender_identity)) = signal::conv_sender_keys(ck) else {
        return;
    };
    let payload = if typing {
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
    let signal = PeerSignal::unsigned(payload);
    signal_all_peers(peers, &signal);
}

fn use_typing_state(key_memo: Memo<String>, identity_memo: Memo<Option<UserCtx>>) -> Memo<bool> {
    let mut tick = use_signal(|| 0u64);
    let version = use_memo(move || {
        TYPING_GENERATION();
        typing_version_for(&key_memo())
    });
    use_resource(move || {
        version();
        let key = key_memo();
        let identity = identity_memo();
        let guard = session::SessionGuard::capture();
        let typing_at = typing_at_for(&key);
        async move {
            if identity.is_none() || typing_at == 0 {
                return;
            }
            let remaining = config::typing::EXPIRY_MS
                .saturating_sub(now_epoch_millis().saturating_sub(typing_at));
            if remaining == 0 {
                return;
            }
            sleep_ms(remaining).await;
            if guard.is_current() && key_memo.peek().as_str() == key {
                tick += 1;
            }
        }
    });

    use_memo(move || {
        tick();
        version();
        identity_memo().is_some() && {
            let typing_at = typing_at_for(&key_memo());
            typing_at > 0
                && now_epoch_millis().saturating_sub(typing_at) < config::typing::EXPIRY_MS
        }
    })
}

/// Marks the conversation as on-screen for the lifetime of the component and
/// arms its session watch. Watches are owned by the session (armed at login
/// for every indexed conversation, cancelled at logout) — the UI only tracks
/// which conversation is open, for presence-signal gating, and opportunistically
/// re-arms the watch in case the login-time arm failed or hasn't run yet.
fn use_watch_lifecycle(key_memo: Memo<String>, identity_memo: Memo<Option<UserCtx>>) {
    let mut previous = use_signal(|| Option::<(Arc<str>, String)>::None);

    use_effect(move || {
        let key = key_memo();
        let identity = identity_memo();
        let next = identity
            .as_ref()
            .map(|ctx| (Arc::clone(&ctx.account_id), key.clone()));
        if *previous.peek() == next {
            return;
        }
        if let Some((old_account, old_key)) = previous.replace(next)
            && identity
                .as_ref()
                .is_none_or(|ctx| ctx.account_id == old_account)
        {
            conversation::clear_meta_entry(&old_key);
            clear_open_conversation(&old_key);
        }
        if identity.is_none() {
            return;
        }
        let guard = session::SessionGuard::capture();
        set_open_conversation(&key);
        conversation::ensure_meta_entry(&key);
        session::spawn_session_detached(async move {
            if guard.is_current() && ensure_watched_if_new(&key).await && guard.is_current() {
                sweep_rearmed_conversation(key.into(), guard).await;
            }
        });
    });

    use_drop(move || {
        if let Some((account_id, key)) = previous.peek().clone()
            && UserCtx::try_current().is_some_and(|ctx| ctx.account_id == account_id)
        {
            conversation::clear_meta_entry(&key);
            clear_open_conversation(&key);
        }
    });
}

/// Store fields projected from the on-screen conversation's snapshot: its
/// messages, the identities to signal, and any load error. One named unit
/// instead of three copies of the same lookup prologue in `use_chat_store`.
struct SnapshotProjections {
    messages: Memo<Arc<Vec<FeedMessage>>>,
    peer_keys: Memo<Vec<String>>,
    local_error: Memo<Option<String>>,
}

fn use_snapshot_projections(
    snapshot: Resource<Option<LocalSnapshot>>,
    key_memo: Memo<String>,
    identity_memo: Memo<Option<UserCtx>>,
    account_id_memo: Memo<Option<Arc<str>>>,
) -> SnapshotProjections {
    // `Arc`'s `PartialEq` short-circuits on pointer identity (std:
    // `ptr::eq(..) || **self == **other`), so a re-load that hands back the
    // *same* `Arc` compares in O(1). It only falls back to the deep
    // element-wise compare when the conversation was rewritten into a new
    // allocation — which is exactly the case where the content did change,
    // so the deep compare is what decides it. `Arc::make_mut` on the write
    // path guarantees a rewritten conversation gets a fresh allocation
    // rather than mutating one in place behind the memo's back.
    let messages = use_memo(move || {
        current_snapshot(
            snapshot.read().as_ref().and_then(Option::as_ref),
            account_id_memo().as_ref(),
        )
        .filter(|snapshot| snapshot.key == key_memo())
        .map_or_else(
            || Arc::new(Vec::new()),
            |snapshot| Arc::clone(&snapshot.messages),
        )
    });
    let peer_keys = use_memo(move || {
        let identity = identity_memo();
        current_snapshot(
            snapshot.read().as_ref().and_then(Option::as_ref),
            account_id_memo().as_ref(),
        )
        .filter(|snapshot| snapshot.key == key_memo())
        .and_then(|snapshot| snapshot.conversation.as_ref().ok())
        .map_or_else(Vec::new, |conv| {
            conv.roster
                .peer_identity_keys(identity.as_ref().map_or("", |ctx| ctx.dht_key.as_ref()))
        })
    });
    let local_error = use_memo(move || {
        current_snapshot(
            snapshot.read().as_ref().and_then(Option::as_ref),
            account_id_memo().as_ref(),
        )
        .filter(|snapshot| snapshot.key == key_memo())
        .and_then(|snapshot| snapshot.conversation.as_ref().err())
        .cloned()
    });
    SnapshotProjections {
        messages,
        peer_keys,
        local_error,
    }
}

pub fn use_chat_store(conversation_key: String, on_message_saved: Callback<Arc<str>>) -> ChatStore {
    let mut key_sig = use_signal(|| conversation_key.clone());
    if *key_sig.peek() != conversation_key {
        key_sig.set(conversation_key);
    }
    let key_memo = use_memo(move || key_sig.read().clone());
    let identity_memo = use_memo(UserCtx::try_current);
    let account_id_memo = use_memo(move || identity_memo().map(|identity| identity.account_id));

    let mut current_key = use_signal(move || key_memo.read().clone());
    let mut current_account = use_signal(|| identity_memo().map(|identity| identity.account_id));
    let mut send_error = use_signal(|| None::<String>);
    let mut load_state = use_signal(ConversationLoadState::default);
    let mut is_sending = use_signal(|| false);

    use_effect(move || {
        let key = key_memo();
        let account = identity_memo().map(|identity| identity.account_id);
        if current_key.peek().as_str() == key.as_str() && *current_account.peek() == account {
            return;
        }
        current_key.set(key);
        current_account.set(account);
        send_error.set(None);
        is_sending.set(false);
        load_state.set(ConversationLoadState::default());
    });

    let mut snapshot = use_local_snapshot(key_memo, identity_memo);
    let mut recovery = use_resource(move || {
        cold_open::recover_conversation(key_memo, identity_memo, current_key, load_state)
    });
    let SnapshotProjections {
        messages,
        peer_keys,
        local_error,
    } = use_snapshot_projections(snapshot, key_memo, identity_memo, account_id_memo);
    let local_loading = use_memo(move || *snapshot.state().read() == UseResourceState::Pending);
    let retry_load = Callback::new(move |()| {
        snapshot.restart();
        recovery.restart();
    });
    let send_message = use_send_message_action(
        key_memo,
        identity_memo,
        current_key,
        send_error,
        is_sending,
        on_message_saved,
    );

    use_watch_lifecycle(key_memo, identity_memo);

    use_drop(move || {
        if identity_memo.peek().as_ref().is_some_and(|expected| {
            UserCtx::try_current().is_some_and(|current| current.account_id == expected.account_id)
        }) {
            // Detached (`spawn_forever`, root scope): Dioxus drains this
            // scope's task queue *before* running drop hooks, so the
            // `spawn_session_scoped` fan-out inside `send_typing_signal`
            // would attach to a dying scope and never be polled — the
            // StopTyping notice would never leave the device.
            let (ck, peers) = (key_memo.peek().clone(), peer_keys.peek().clone());
            session::spawn_session_detached(async move {
                send_typing_signal(&ck, &peers, false);
            });
        }
    });
    let is_typing = use_typing_state(key_memo, identity_memo);
    let send_typing = Callback::new(move |()| {
        let (ck, cached_peers) = (key_memo(), peer_keys.peek().clone());
        session::spawn_session_scoped(async move {
            send_typing_signal(&ck, &cached_peers, true);
        });
    });
    ChatStore {
        messages,
        is_typing,
        send_error,
        load_state,
        local_loading,
        local_error,
        is_sending,
        retry_load,
        send_message,
        send_typing,
    }
}
