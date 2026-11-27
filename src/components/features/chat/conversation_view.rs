//! Conversation view with `DaisyUI` chat bubbles.
//!
//! Message rendering is split into a separate [`MessageBubble`] component so
//! Dioxus can skip re-diffing messages whose props have not changed (props
//! derive `PartialEq`). This turns a full O(n) diff on every new message into
//! O(1) for the existing messages plus one new `VNode`.

use std::{cell::Cell, rc::Rc, sync::Arc};

use dioxus::{logger::tracing::warn, prelude::*};

use crate::{
    chat::{feed::MessageStatus, hooks::use_chat_store, retry_message_delivery},
    components::shared::{CenteredPage, RetryButton, format_local_ymdhm_from_millis},
    config,
    global_state::network_status,
    helpers::async_utils::now_epoch_millis,
    router::Route,
    session::{
        epoch::{SessionGuard, spawn_session_scoped},
        phase::SESSION,
    },
    user::User,
};

// ── Memoized message bubble ───────────────────────────────────────────────

/// A single chat bubble. Because all props are `PartialEq`, Dioxus skips
/// re-rendering this component when the underlying message data has not changed.
#[component]
fn MessageBubble(
    conversation_key: Arc<str>,
    message_id: Arc<str>,
    sender_username: Arc<str>,
    content: Arc<str>,
    timestamp: u64,
    is_mine: bool,
    status: MessageStatus,
    mut send_error: Signal<Option<String>>,
) -> Element {
    let (bubble_class, color_class) = if is_mine {
        ("chat chat-end", "chat-bubble chat-bubble-primary")
    } else {
        ("chat chat-start", "chat-bubble")
    };
    let is_failed = is_mine && status == MessageStatus::Failed;
    let is_pending = is_mine && status == MessageStatus::Pending;

    rsx! {
        div { class: bubble_class,
            if !is_mine {
                {
                    let time_str = format_local_ymdhm_from_millis(timestamp);
                    rsx! {
                        div { class: "chat-header",
                            "{sender_username}"
                            time { class: "opacity-60 ml-1", "{time_str}" }
                        }
                    }
                }
            }
            div {
                class: if is_failed { "chat-bubble bg-error/10 text-base-content/60" } else if is_pending { "chat-bubble chat-bubble-primary opacity-70" } else { color_class },
                "{content}"
            }
            if is_pending {
                div { class: "chat-footer text-base-content/60 flex items-center gap-1",
                    span { class: "icon-[heroicons--clock]", "aria-hidden": "true" }
                    "Pending"
                }
            } else if is_failed {
                div { class: "chat-footer text-error flex items-center gap-2",
                    "Not delivered"
                    button {
                        class: "link link-error",
                        r#type: "button",
                        onclick: move |_| {
                            let conversation_key = conversation_key.clone();
                            let message_id = Arc::clone(&message_id);
                            let guard = SessionGuard::capture();
                            send_error.set(None);
                            spawn_session_scoped(async move {
                                if !guard.is_current() {
                                    return;
                                }
                                if let Err(error) = retry_message_delivery(&conversation_key, &message_id).await {
                                    warn!("Failed to retry message delivery: {error:#}");
                                    guard.commit(|| {
                                        send_error.set(Some("Message could not be queued. Please try again.".to_string()));
                                    });
                                }
                            });
                        },
                        "Retry"
                    }
                }
            }
        }
    }
}

#[component]
fn TypingIndicator(is_typing: Memo<bool>) -> Element {
    if !is_typing() {
        return rsx! {};
    }
    rsx! {
        div { class: "chat chat-start",
            div { class: "chat-bubble opacity-50",
                span { class: "loading loading-dots" }
            }
        }
    }
}

#[component]
fn MessageComposer(
    mut input_text: Signal<String>,
    is_sending: ReadSignal<bool>,
    send_message: Callback<String>,
    send_typing: Callback<()>,
    mut send_internal_error: Signal<Option<String>>,
) -> Element {
    let last_typing_sent = use_hook(|| Cell::new(0u64));
    let mut do_send = move || {
        // C-P5: trim-check before allocating the owned String.
        let guard = input_text.read();
        let trimmed = guard.trim();
        if trimmed.is_empty() || is_sending() {
            return;
        }
        let text = trimmed.to_string();
        drop(guard);
        send_internal_error.set(None);
        send_message.call(text);
    };

    rsx! {
        div { class: "flex gap-2 mt-2",
            textarea {
                class: "textarea flex-1 resize-none",
                "aria-label": "Message",
                rows: "2",
                placeholder: "Type a message...",
                value: input_text,
                oninput: move |ev| {
                    send_internal_error.set(None);
                    input_text.set(ev.value());
                    let now = now_epoch_millis();
                    if now - last_typing_sent.get() > 3000 {
                        last_typing_sent.set(now);
                        send_typing.call(());
                    }
                },
                onkeydown: move |ev| {
                    if ev.key() == Key::Named(NamedKey::Enter) && !ev.modifiers().shift() {
                        ev.prevent_default();
                        do_send();
                    }
                },
            }
            // daisyUI `aura`: rotating ring plus blurred halo, sized from the
            // child's --radius-field. Hidden while the action is disabled.
            div { class: "aura aura-glow self-end",
                button {
                    class: "btn btn-sm btn-secondary",
                    r#type: "button",
                    "aria-label": "Send message",
                    disabled: is_sending() || input_text.read().trim().is_empty(),
                    onclick: move |_| do_send(),
                    span { class: "icon-[heroicons--paper-airplane]" }
                }
            }
        }
        if let Some(err) = send_internal_error.read().as_ref() {
            p { class: "validator-hint text-error mt-1", "{err}" }
        }
    }
}

#[component]
pub fn ConversationView(conversation_key: String) -> Element {
    let mut input_text = use_signal(String::new);
    use_effect(use_reactive((&conversation_key,), move |_| {
        input_text.set(String::new());
    }));

    // ── History window ────────────────────────────────────────────────────
    //
    // The view renders only the newest slice of the history and grows it when
    // the reader scrolls up, so a 2,000-message conversation does not put
    // 2,000 bubbles in the DOM at once. `window_count` is how many messages
    // are rendered, counted from the newest end; the rendered slice is
    // `message_list[message_list.len() - window_count..]`.
    //
    // The scroll container is `flex-col-reverse`: the DOM order is
    // newest-first and the *visually oldest* end is the end of the DOM. The
    // "load older" trigger is therefore a sentinel element rendered at the
    // end of the DOM (the visual top), and growth is detected by asking
    // whether that sentinel has come into view.
    //
    // The sentinel's *rect* is used rather than `scroll_top`: the two
    // renderers disagree on the sign convention for a reversed column
    // (Chromium reports a negative `scrollTop` with 0 at the newest end,
    // while the native renderer clamps the offset to `[0, max]`), so
    // sign-dependent arithmetic would only be correct on one of them. A
    // client rectangle is a plain geometric comparison that means the same
    // thing on both.
    let mut window_count = use_signal(|| config::chat::WINDOW_MESSAGES);
    // Reset the window when the conversation changes: a different
    // conversation has its own history, so the old window size must not
    // carry over.
    use_effect(use_reactive((&conversation_key,), move |_| {
        window_count.set(config::chat::WINDOW_MESSAGES);
    }));
    // The sentinel's mounted node, used to ask whether it has scrolled into
    // view. `None` until the first render that has older messages to offer.
    let mut older_sentinel_el: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    // The scroll container, used to keep the reader's place when the window
    // grows.
    let mut scroll_el: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    // Guards against overlapping growths: the sentinel stays in view until
    // the newly loaded page is rendered, so without this a single scroll
    // would start several loads at once.
    let mut growing = use_signal(|| false);

    // Initialize chat store
    let bubble_conversation_key = use_memo(use_reactive((&conversation_key,), |(k,)| {
        Arc::<str>::from(k.as_str())
    }));
    let on_saved = Callback::new(move |saved: Arc<str>| {
        let unchanged = input_text.peek().trim() == saved.as_ref();
        if unchanged {
            input_text.set(String::new());
        }
    });
    let chat_store = use_chat_store(conversation_key, on_saved);
    let messages = chat_store.messages;
    let is_typing = chat_store.is_typing;
    let send_message = chat_store.send_message;
    let send_typing = chat_store.send_typing;
    let is_sending = chat_store.is_sending;
    let retry_load = chat_store.retry_load;
    let send_internal_error = chat_store.send_error;
    let loading_head = use_memo(move || chat_store.load_state.read().loading_head);
    let history_loading = use_memo(move || chat_store.load_state.read().loading_history);
    let local_loading = chat_store.local_loading;
    let load_error = use_memo(move || {
        (chat_store.local_error)().or_else(|| chat_store.load_state.read().error.clone())
    });
    let net_state = use_memo(|| {
        let n = network_status().read();
        (n.is_attached, n.has_connectivity())
    });
    let activity = use_memo(move || {
        if is_sending() {
            Some(("Saving message", true))
        } else if local_loading() && messages.read().is_empty() {
            Some(("Loading saved messages", true))
        } else if loading_head() {
            Some(("Loading conversation", true))
        } else if history_loading() {
            Some(("Loading older messages", true))
        } else {
            let (is_attached, has_connectivity) = net_state();
            (!has_connectivity).then_some((
                if is_attached {
                    "Connecting - messages stay queued"
                } else {
                    "Offline - messages stay queued"
                },
                false,
            ))
        }
    });

    let identity_memo =
        use_memo(move || SESSION.read().authenticated_user().map(User::username_arc));

    rsx! {
        CenteredPage {
            contained: true,
            // `CenteredPage`'s own wrapper caps the column at `max-w-sm`
            // (384px), which leaves ~450px of dead space on both sides of a
            // desktop window. `max-w-2xl` keeps the reading measure sane on a
            // phone (the viewport caps it there) and fills the pane on a
            // desktop.
            width: "max-w-2xl",
            div { class: "w-full max-w-2xl min-h-0 flex flex-col h-full",
                div { class: "flex items-center gap-3 mb-3",
                    Link {
                        class: "btn btn-sm btn-ghost btn-circle",
                        to: Route::Chat,
                        aria_label: "Back to chats",
                        span { class: "icon-[heroicons--arrow-left]", "aria-hidden": "true" }
                    }
                    h1 { class: "flex-1", "Chat" }
                }

                // Messages area. `flex-col-reverse` with newest-first DOM order
                // makes the browser pin scroll to the bottom natively (scroll
                // origin is the bottom edge): new messages keep the view
                // pinned, while a user who scrolled up to read history stays
                // where they are — no scroll scripting needed.
                div {
                    id: "chat-messages",
                    class: "min-h-0 flex-1 overflow-y-auto flex flex-col-reverse gap-2 p-2",
                    onmounted: move |ev| {
                        scroll_el.set(Some(ev.data()));
                    },
                    onscroll: move |_ev: ScrollEvent| {
                        // Growth is driven by the sentinel: when it scrolls
                        // into view, the reader has reached the oldest
                        // rendered message and the next older page is loaded.
                        //
                        // The check is asynchronous because the sentinel's
                        // client rectangle is only available once the
                        // renderer has laid it out.
                        let Some(sentinel) = older_sentinel_el.peek().clone() else {
                            return;
                        };
                        let Some(container) = scroll_el.peek().clone() else {
                            return;
                        };
                        let total = messages.read().len();
                        let current = *window_count.peek();
                        if current >= total {
                            return;
                        }
                        // Only one growth may be in flight: the sentinel is
                        // still in view until the new page is rendered, so
                        // without this guard a single scroll would start
                        // several overlapping loads.
                        if *growing.peek() {
                            return;
                        }
                        growing.set(true);
                        spawn(async move {
                            let sentinel_rect = sentinel.get_client_rect().await;
                            let container_rect = container.get_client_rect().await;
                            if let (Ok(sentinel_rect), Ok(container_rect)) =
                                (sentinel_rect, container_rect)
                            {
                                // The sentinel is the DOM's last child, so in
                                // the reversed column it is the visually
                                // top-most element: the oldest end of the
                                // history. It has been "reached" once it is
                                // inside the container's visible rectangle.
                                //
                                // This is a plain geometric comparison, so it
                                // means the same thing on every renderer —
                                // unlike the scroll offset, whose origin for a
                                // reversed column differs between the browser
                                // (measured from the newest edge) and the
                                // native engine (measured from the top).
                                let reached = sentinel_rect.max_y() >= container_rect.min_y()
                                    && sentinel_rect.min_y() <= container_rect.max_y();
                                if reached {
                                    // Grow the window. The reader's place is
                                    // kept by the renderer's own scroll
                                    // anchoring: the older bubbles are added
                                    // at the visual top of the reversed
                                    // column, and the browser keeps the
                                    // content the reader is looking at in
                                    // place while it does so.
                                    //
                                    // No explicit offset correction is made.
                                    // Measured in Chromium, the content
                                    // already stays put across a growth
                                    // (the sentinel moves by the height of
                                    // the inserted page, but the messages
                                    // under the reader do not), so adding a
                                    // correction would move the reader away
                                    // from what they were reading.
                                    let next =
                                        (current + config::chat::WINDOW_PAGE).min(total);
                                    window_count.set(next);
                                }
                            }
                            growing.set(false);
                        });
                    },
                    {
                        let my_username = identity_memo();
                        let message_list = messages.read();
                        if message_list.is_empty() {
                            if local_loading() || loading_head() || load_error().is_some() {
                                rsx! {}
                            } else { rsx! {
                                div { class: "text-center text-base-content/50 py-8 my-auto",
                                    p { "No messages yet." }
                                    p { class: "mt-1", "Send a message to start the conversation." }
                                }
                            } }
                        } else {
                            // Only the newest `window_count` messages are
                            // rendered; older ones are added as the reader
                            // scrolls up. A conversation shorter than the
                            // window renders every message, exactly as
                            // before.
                            let total = message_list.len();
                            // `read()` (not `peek()`) so this render scope
                            // subscribes to the signal: the window grows by
                            // writing it, and the new slice has to be
                            // re-rendered for the growth to be visible.
                            let count = window_count.read().min(total);
                            let start = total - count;
                            let has_older = start > 0;
                            // The sentinel's key carries the number of hidden
                            // older messages, so it changes whenever the
                            // window grows and the element is re-created
                            // rather than diffed in place.
                            let older_sentinel_key = format!("older-{start}");
                            rsx! {
                                // Bottom-most element first in DOM (column-reverse).
                                TypingIndicator { is_typing }

                                for msg in message_list[start..].iter().rev() {
                                    MessageBubble {
                                        key: "{msg.msg_id}",
                                        conversation_key: bubble_conversation_key(),
                                        message_id: Arc::clone(&msg.msg_id),
                                        sender_username: Arc::clone(&msg.sender_username),
                                        content: Arc::clone(&msg.content),
                                        timestamp: msg.timestamp,
                                        is_mine: my_username.as_ref().is_some_and(|u| u.as_ref() == msg.sender_username.as_ref()),
                                        status: msg.status,
                                        send_error: send_internal_error,
                                    }
                                }

                                if has_older {
                                    div {
                                        key: "{older_sentinel_key}",
                                        class: "text-center text-xs text-base-content/50 py-2",
                                        onmounted: move |ev| {
                                            older_sentinel_el.set(Some(ev.data()));
                                        },
                                        "Scroll up for older messages"
                                    }
                                }
                            }
                        }
                    }
                }

                if let Some((label, busy)) = activity() {
                    details { class: "text-xs text-base-content/60 mt-2",
                        summary { class: "flex items-center gap-2 cursor-pointer",
                            if busy {
                                span { class: "loading loading-spinner loading-xs", "aria-hidden": "true" }
                            }
                            span { role: "status", "{label}" }
                        }
                        p { class: "mt-1",
                            "Messages are saved on this device before network delivery. Queued delivery and history recovery continue in the background."
                        }
                    }
                }
                if let Some(err) = load_error().as_ref() {
                    div { class: "flex items-center gap-2 mt-1",
                        p { class: "text-error text-xs", role: "alert", "{err}" }
                        RetryButton {
                            class: "btn-xs",
                            disabled: local_loading() || loading_head() || history_loading(),
                            onclick: move |()| retry_load.call(()),
                        }
                    }
                }

                MessageComposer {
                    input_text,
                    is_sending,
                    send_message,
                    send_typing,
                    send_internal_error,
                }
            }
        }
    }
}
