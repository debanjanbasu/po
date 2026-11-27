//! Chat contacts list with add contact flow.

use dioxus::prelude::*;

use std::{collections::HashMap, sync::Arc};

use crate::{
    chat::{
        AccountScoped,
        contact::{ChatContact, ContactList},
        conversation::ConversationIndex,
        current_snapshot,
        invite::{PENDING_INVITES, accept_pending_invite_by_key, reject_pending_invite_by_key},
        session_flow::retry_pending_contact,
    },
    components::shared::{
        CenteredPage, FeedbackKind, FormFeedback, RetryButton, format_local_ymdhm_from_millis,
    },
    router::Route,
    session::{
        epoch::{SessionGuard, spawn_session_scoped},
        phase::SESSION,
    },
    storage::{
        background_errors::report_nonfatal,
        store_trait::{self, use_model_revision},
    },
    user::User,
};

use super::add_contact::AddContact;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ContactFilter {
    #[default]
    All,
    Active,
    Pending,
}

fn matches_filter(contact: &ChatContact, filter: ContactFilter) -> bool {
    match filter {
        ContactFilter::All => true,
        ContactFilter::Active => !contact.conversation_record_key.is_empty(),
        ContactFilter::Pending => contact.conversation_record_key.is_empty(),
    }
}

type ConversationMetaLookup = Arc<HashMap<String, (Option<String>, u32)>>;

struct AccountSnapshot<T> {
    account_id: Arc<str>,
    data: Result<T, &'static str>,
}

impl<T> AccountScoped for AccountSnapshot<T> {
    fn account_id(&self) -> &Arc<str> {
        &self.account_id
    }
}

/// C-S2: shared avatar bubble used by both pending and active contact rows.
#[component]
fn ContactAvatar(display_name: Arc<str>) -> Element {
    let initial = display_name.chars().next().unwrap_or('?');
    rsx! {
        div { class: "avatar avatar-placeholder",
            div { class: "bg-primary text-primary-content rounded-full w-10",
                span { "{initial}" }
            }
        }
    }
}

/// Isolated pending-invites section so only this component subscribes to
/// `PENDING_INVITES` — invite sync no longer re-renders the whole chat list.
#[component]
fn PendingInvites() -> Element {
    let mut invite_error_key = use_signal(|| None::<String>);
    let mut active_action = use_signal(|| None::<(String, bool)>);
    let account_id = use_memo(move || {
        SESSION
            .read()
            .authenticated_user()
            .map(User::account_id_arc)
    });
    use_effect(move || {
        account_id();
        invite_error_key.set(None);
        active_action.set(None);
    });
    let update_invite = Callback::new(move |(key, accept): (String, bool)| {
        if active_action.peek().is_some() {
            return;
        }
        let guard = SessionGuard::capture();
        invite_error_key.set(None);
        active_action.set(Some((key.clone(), accept)));
        spawn_session_scoped(async move {
            let result = if accept {
                accept_pending_invite_by_key(&key).await
            } else {
                reject_pending_invite_by_key(&key).await
            };
            if let Err(error) = &result {
                report_nonfatal("chat.invite_ui", "Failed to update pending invite", error);
            }
            guard.commit(|| {
                active_action.set(None);
                if result.is_err() {
                    invite_error_key.set(Some(key));
                }
            });
        });
    });

    let invites = PENDING_INVITES.read();
    if invites.is_empty() || account_id().is_none() {
        return rsx! {};
    }
    let active = active_action.read();
    let error_key = invite_error_key.read();
    rsx! {
        div { class: "mb-4 space-y-2",
            h2 { class: "text-sm font-semibold text-base-content/70", "Pending Invites" }
            for invite in invites.iter() {
                {
                    let is_accepting = active
                        .as_ref()
                        .is_some_and(|(key, accept)| *key == invite.manifest_key_str && *accept);
                    let is_dismissing = active
                        .as_ref()
                        .is_some_and(|(key, accept)| *key == invite.manifest_key_str && !*accept);
                    rsx! {
                        div {
                            key: "{invite.manifest_key_str}",
                            class: "flex flex-wrap items-center gap-3 p-3 rounded-field bg-base-200",
                            div { class: "avatar avatar-placeholder",
                                div { class: "bg-accent text-accent-content rounded-full w-10",
                                    span { class: "icon-[line-md--account]" }
                                }
                            }
                            div { class: "flex-1 min-w-0",
                                p { class: "truncate text-sm", "Unknown sender" }
                                p { class: "text-xs text-base-content/50 truncate",
                                    "{format_local_ymdhm_from_millis(invite.received_at)}"
                                }
                            }
                            button {
                                class: "btn btn-xs btn-success",
                                disabled: active.is_some(),
                                onclick: {
                                    let key = invite.manifest_key_str.clone();
                                    move |_| update_invite.call((key.clone(), true))
                                },
                                if is_accepting { "Accepting..." } else { "Accept" }
                            }
                            button {
                                class: "btn btn-xs btn-ghost",
                                disabled: active.is_some(),
                                onclick: {
                                    let key = invite.manifest_key_str.clone();
                                    move |_| update_invite.call((key.clone(), false))
                                },
                                if is_dismissing { "Dismissing..." } else { "Dismiss" }
                            }
                            if error_key.as_ref().is_some_and(|k| *k == invite.manifest_key_str) {
                                p { class: "text-error text-xs w-full mt-1", "Invite update could not complete. Try again." }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A pending contact (conversation setup not finished yet).
///
/// Split into its own component so that a preview or unread-count change
/// for *another* conversation does not re-render this row. All props are
/// `PartialEq`, so Dioxus skips the render when nothing about the row
/// changed.
#[component]
fn PendingContactRow(
    contact: ChatContact,
    is_retrying: bool,
    any_retry_in_flight: bool,
    retry_failed: bool,
    retry_contact: Callback<ChatContact>,
) -> Element {
    rsx! {
        div {
            class: "flex flex-wrap items-center gap-3 p-3 rounded-field bg-base-200",
            ContactAvatar { display_name: contact.display_name.clone() }
            div { class: "flex-1 min-w-0",
                p { class: "truncate", "{contact.display_name}" }
                p { class: "text-base-content/70 truncate", "@{contact.username}" }
                p { class: "text-xs text-base-content/70", "Conversation setup pending" }
            }
            button {
                class: "btn btn-sm btn-warning",
                r#type: "button",
                // A retry is already queued for some contact: the whole list
                // stays disabled until it settles, exactly as before.
                disabled: any_retry_in_flight,
                onclick: move |_| retry_contact.call(contact.clone()),
                span { class: "icon-[line-md--rotate-270]" }
                if is_retrying {
                    "Retrying..."
                } else {
                    "Retry"
                }
            }
            if retry_failed {
                p { class: "text-error text-xs w-full",
                    "Setup could not be queued. Try again."
                }
            }
        }
    }
}

/// An active contact with a conversation.
#[component]
fn ActiveContactRow(
    display_name: Arc<str>,
    username: Arc<str>,
    conversation_key: Arc<str>,
    preview: Option<String>,
    unread: u32,
) -> Element {
    rsx! {
        Link {
            class: "flex items-center gap-3 p-3 rounded-field hover:bg-base-200 transition-colors cursor-pointer",
            to: Route::ChatConversation { conversation_key: conversation_key.to_string() },
            ContactAvatar { display_name: display_name.clone() }
            div { class: "flex-1 min-w-0",
                p { class: "truncate", "{display_name}" }
                if let Some(text) = preview {
                    p { class: "text-base-content/60 truncate", "{text}" }
                } else {
                    p { class: "text-base-content/60 truncate", "@{username}" }
                }
            }
            if unread > 0 {
                span { class: "badge badge-primary", "{unread}" }
            }
        }
    }
}

/// Main chat page showing contacts list.
#[component]
pub fn ChatList() -> Element {
    let mut show_add = use_signal(|| false);
    let mut filter = use_signal(ContactFilter::default);
    let mut retrying_contact = use_signal(|| None::<Arc<str>>);
    let mut retry_error_key = use_signal(|| None::<Arc<str>>);

    let account_id_memo = use_memo(move || {
        SESSION
            .read()
            .authenticated_user()
            .map(User::account_id_arc)
    });
    use_effect(move || {
        account_id_memo();
        retrying_contact.set(None);
        retry_error_key.set(None);
        show_add.set(false);
    });
    let retry_contact = Callback::new(move |contact: ChatContact| {
        if retrying_contact.peek().is_some() {
            return;
        }
        let guard = SessionGuard::capture();
        retry_error_key.set(None);
        retrying_contact.set(Some(Arc::clone(&contact.dht_key)));
        spawn_session_scoped(async move {
            let result = retry_pending_contact(&contact).await;
            if let Err(error) = &result {
                report_nonfatal(
                    "chat.chat_list",
                    "Failed to retry conversation setup",
                    error,
                );
            }
            guard.commit(|| {
                retrying_contact.set(None);
                if result.is_err() {
                    retry_error_key.set(Some(contact.dht_key));
                }
            });
        });
    });

    let contacts_revision = use_model_revision::<ContactList>();
    let conversations_revision = use_model_revision::<ConversationIndex>();
    let mut contacts_resource = use_resource(move || {
        contacts_revision();
        let account_id = account_id_memo();
        async move {
            let account_id = account_id?;
            let data = store_trait::load_local::<ContactList>(account_id.as_bytes())
                .await
                .map(|list| {
                    let mut contacts: Vec<_> = list.contacts.into_values().collect();
                    contacts.sort_by_cached_key(|contact| {
                        (
                            contact.display_name.to_lowercase(),
                            Arc::clone(&contact.dht_key),
                        )
                    });
                    Arc::<[ChatContact]>::from(contacts)
                })
                .map_err(|error| {
                    report_nonfatal("chat.chat_list", "Failed to load contacts", error);
                    "Contacts could not be loaded. Please try again."
                });
            Some(AccountSnapshot { account_id, data })
        }
    });
    let mut conversations_resource = use_resource(move || {
        conversations_revision();
        let account_id = account_id_memo();
        async move {
            let account_id = account_id?;
            let data = store_trait::load_local::<ConversationIndex>(account_id.as_bytes())
                .await
                .map(|index| {
                    Arc::new(
                        index
                            .conversations
                            .into_values()
                            .map(|entry| {
                                (
                                    entry.conversation_record_key,
                                    (
                                        (!entry.last_message_preview.is_empty())
                                            .then_some(entry.last_message_preview),
                                        entry.unread_count,
                                    ),
                                )
                            })
                            .collect(),
                    )
                })
                .map_err(|error| {
                    report_nonfatal("chat.chat_list", "Failed to load conversations", error);
                    "Conversations could not be loaded. Please try again."
                });
            Some(AccountSnapshot { account_id, data })
        }
    });

    let filtered_contacts = use_memo(move || {
        let active_filter = filter();
        current_snapshot(
            contacts_resource.read().as_ref().and_then(Option::as_ref),
            account_id_memo().as_ref(),
        )
        .and_then(|snapshot| snapshot.data.as_ref().ok())
        .map_or_else(
            || Arc::from([]),
            |contacts| {
                if active_filter == ContactFilter::All {
                    Arc::clone(contacts)
                } else {
                    contacts
                        .iter()
                        .filter(|contact| matches_filter(contact, active_filter))
                        .cloned()
                        .collect::<Arc<[ChatContact]>>()
                }
            },
        )
    });
    let conv_meta_lookup = use_memo(move || {
        current_snapshot(
            conversations_resource
                .read()
                .as_ref()
                .and_then(Option::as_ref),
            account_id_memo().as_ref(),
        )
        .and_then(|snapshot| snapshot.data.as_ref().ok())
        .map_or_else(ConversationMetaLookup::default, Arc::clone)
    });
    let error = use_memo(move || {
        let account_id = account_id_memo();
        let contacts_err = current_snapshot(
            contacts_resource.read().as_ref().and_then(Option::as_ref),
            account_id.as_ref(),
        )
        .and_then(|snapshot| snapshot.data.as_ref().err())
        .copied();
        contacts_err.or_else(|| {
            current_snapshot(
                conversations_resource
                    .read()
                    .as_ref()
                    .and_then(Option::as_ref),
                account_id.as_ref(),
            )
            .and_then(|snapshot| snapshot.data.as_ref().err())
            .copied()
        })
    });
    let loading = use_memo(move || {
        *contacts_resource.state().read() == UseResourceState::Pending
            || *conversations_resource.state().read() == UseResourceState::Pending
    });
    let has_filtered_contacts = use_memo(move || !filtered_contacts.read().is_empty());

    rsx! {
        CenteredPage { contained: true,
            div { class: "flex items-center justify-between mb-4",
                h1 { "Chat" }
                button {
                    id: "chat-add-contact-button",
                    class: "btn btn-sm btn-secondary",
                    onclick: move |_| show_add.set(true),
                    span { class: "icon-[line-md--person-add-twotone]" }
                    "Add Contact"
                }
            }

            if let Some(err) = error() {
                div { class: "flex items-center gap-2 mb-3",
                    FormFeedback {
                        kind: FeedbackKind::Error,
                        message: err.to_string(),
                        compact: true,
                    }
                    RetryButton {
                        class: "btn-xs",
                        disabled: loading(),
                        onclick: move |()| {
                            contacts_resource.restart();
                            conversations_resource.restart();
                        },
                    }
                }
            }


                if show_add() {
                    AddContact {
                        on_close: move |()| {
                            show_add.set(false);
                        },
                    }
                }

                PendingInvites {}

                div { class: "filter mb-3",
                    input {
                        r#type: "radio",
                        name: "contact-filter",
                        class: "btn btn-sm filter-reset",
                        "aria-label": "All",
                        checked: filter() == ContactFilter::All,
                        onchange: move |_| filter.set(ContactFilter::All),
                    }
                    input {
                        r#type: "radio",
                        name: "contact-filter",
                        class: "btn btn-sm",
                        "aria-label": "Active",
                        checked: filter() == ContactFilter::Active,
                        onchange: move |_| filter.set(ContactFilter::Active),
                    }
                    input {
                        r#type: "radio",
                        name: "contact-filter",
                        class: "btn btn-sm",
                        "aria-label": "Pending",
                        checked: filter() == ContactFilter::Pending,
                        onchange: move |_| filter.set(ContactFilter::Pending),
                    }
                }

                if loading() {
                    p { class: "sr-only", role: "status", "Loading saved chats..." }
                    div { class: "space-y-1", "aria-hidden": "true",
                        for index in 0_u8..3 {
                            div {
                                key: "{index}",
                                class: "flex items-center gap-3 p-3 rounded-field bg-base-200",
                                div { class: "skeleton w-10 h-10 rounded-full" }
                                div { class: "flex-1 min-w-0",
                                    p { class: "skeleton skeleton-text truncate",
                                        "Loading contacts"
                                    }
                                    p { class: "skeleton skeleton-text truncate",
                                        "Loading saved chats"
                                    }
                                }
                            }
                        }
                    }
                }
                if !has_filtered_contacts() && !loading() && error().is_none() {
                    div { class: "text-center py-8 text-base-content/60",
                        p { "No contacts yet." }
                        p { class: "mt-2", "Share your QR code to connect with someone." }
                    }
                } else if has_filtered_contacts() {
                    {
                        let conv_meta = conv_meta_lookup();
                        let retrying = retrying_contact.read();
                        let retry_err = retry_error_key.read();
                        rsx! {
                            div { class: "space-y-1",
                                for contact in filtered_contacts.read().iter() {
                                    {
                                        let conv_key = contact.conversation_record_key.as_ref();
                                        let meta = conv_meta.get(conv_key);
                                        let preview = meta.and_then(|(p, _)| p.as_deref());
                                        let unread = meta.map_or(0, |(_, u)| *u);
                                        if conv_key.is_empty() {
                                            rsx! {
                                                PendingContactRow {
                                                    key: "{contact.dht_key}",
                                                    contact: contact.clone(),
                                                    is_retrying: retrying.as_ref() == Some(&contact.dht_key),
                                                    any_retry_in_flight: retrying.is_some(),
                                                    retry_failed: retry_err.as_ref() == Some(&contact.dht_key),
                                                    retry_contact,
                                                }
                                            }
                                        } else {
                                            rsx! {
                                                ActiveContactRow {
                                                    key: "{contact.dht_key}",
                                                    display_name: contact.display_name.clone(),
                                                    username: contact.username.clone(),
                                                    conversation_key: contact.conversation_record_key.clone(),
                                                    preview: preview.map(str::to_owned),
                                                    unread,
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
    }
}
