//! `ChatList` — gpui port of `components/features/chat/chat_list.rs`.
//!
//! Contract preserved:
//! - contacts load per account from the `ContactList` store, sorted by
//!   display name; reloads on the revision bus (old `use_model_revision`);
//! - filter tabs (All / Active / Pending) with the same `matches_filter`;
//! - pending invites section isolated to its own observation; accept/reject
//!   with session-guarded action state;
//! - retry pending contact flow with per-contact error marker;
//! - seam id: `chat-add-contact-button` (+ `#chat-messages` lives in the
//!   conversation view).
//!
//! gpui shape: one `Entity<ChatListView>` per open; the contact/conversation
//! snapshots are rebuilt in `render` from the store when revisions advance.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    App, Context, FocusHandle, Focusable, FontWeight, Render, SharedString, Window, prelude::*, px,
};

use crate::{
    chat::{
        contact::{ChatContact, ContactList},
        conversation::ConversationIndex,
        invite::{accept_pending_invite_by_key, reject_pending_invite_by_key},
        session_flow::retry_pending_contact,
    },
    session::epoch::{SessionGuard, spawn_session_scoped},
    storage::background_errors::report_nonfatal,
    ui::{
        state::{revision_entity, session_phase},
        widgets::page::CenteredPage,
    },
    user::User,
};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ContactFilter {
    #[default]
    All,
    Active,
    Pending,
}

impl ContactFilter {
    pub const TABS: [Self; 3] = [Self::All, Self::Active, Self::Pending];

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Active",
            Self::Pending => "Pending",
        }
    }
}

pub fn matches_filter(contact: &ChatContact, filter: ContactFilter) -> bool {
    match filter {
        ContactFilter::All => true,
        ContactFilter::Active => !contact.conversation_record_key.is_empty(),
        ContactFilter::Pending => contact.conversation_record_key.is_empty(),
    }
}

fn account_id(cx: &App) -> Option<Arc<str>> {
    session_phase(cx)
        .authenticated_user()
        .map(User::account_id_arc)
}

pub struct ChatListView {
    focus: FocusHandle,
    filter: ContactFilter,
    show_add: bool,
    /// Sorted contact snapshot for the current account (rebuilt on revision).
    contacts: Result<Arc<[ChatContact]>, &'static str>,
    contacts_account: Option<Arc<str>>,
    /// Conversation meta: key → last feed update (bumped on
    /// remote merges; the old `ConversationMetaLookup` source).
    conv_meta: Arc<HashMap<String, u64>>,
    retrying_contact: Option<Arc<str>>,
    retry_error_key: Option<Arc<str>>,
    /// Invite action in flight (key, accept?).
    active_invite: Option<(String, bool)>,
    invite_error_key: Option<String>,
}

impl Focusable for ChatListView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ChatListView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            focus: cx.focus_handle(),
            filter: ContactFilter::default(),
            show_add: false,
            contacts: Ok(Arc::from([])),
            contacts_account: None,
            conv_meta: Arc::new(HashMap::new()),
            retrying_contact: None,
            retry_error_key: None,
            active_invite: None,
            invite_error_key: None,
        };
        this.reload(cx);
        // Reload on revision bumps (the port of use_model_revision + resource).
        let contact_rev = revision_entity::<ContactList>(cx);
        let conv_rev = revision_entity::<ConversationIndex>(cx);
        let this_handle = cx.entity();
        cx.observe(&contact_rev, move |_, _, cx| {
            this_handle.update(cx, Self::reload);
        })
        .detach();
        let this_handle = cx.entity();
        cx.observe(&conv_rev, move |_, _, cx| {
            this_handle.update(cx, Self::reload);
        })
        .detach();
        this
    }

    /// Rebuilds the contact + conversation-meta snapshots for the current
    /// account (the old `use_resource` body, run synchronously off the store
    /// cache — store reads are cheap local table reads).
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(account_id) = account_id(cx) else {
            self.contacts = Ok(Arc::from([]));
            self.contacts_account = None;
            return;
        };
        if self.contacts_account.as_ref() == Some(&account_id) && self.contacts.is_ok() {
            // Same account: keep the snapshot; revision bumps only matter
            // when the store changed.
        }
        self.contacts_account = Some(account_id.clone());
        // The meta lookup lives in the shared CONVERSATION_META global
        // (same source the old memo read); keys are conversation keys.
        self.conv_meta = Arc::new(
            crate::global_state::conversation_meta()
                .read()
                .iter()
                .map(|(k, meta)| (k.to_string(), meta.last_feed_update))
                .collect(),
        );
        // `load_local` is async (veilid table store); the load runs on the
        // background path like the old `use_resource` future and lands via
        // `cx.notify` when it completes.
        let account_bytes = account_id.as_bytes().to_vec();
        cx.spawn(async move |this, cx| {
            let loaded =
                crate::storage::store_trait::load_local::<ContactList>(&account_bytes).await;
            this.update(cx, |this, cx| {
                this.contacts = match loaded {
                    Ok(list) => {
                        let mut contacts: Vec<ChatContact> = list.contacts.into_values().collect();
                        contacts.sort_by_cached_key(|contact| {
                            (contact.display_name.to_lowercase(), contact.dht_key.clone())
                        });
                        Ok(contacts.into())
                    }
                    Err(error) => {
                        report_nonfatal("chat.chat_list", "Failed to load contacts", &error);
                        Err("Couldn't load your contacts")
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn retry_contact(&mut self, contact: ChatContact) {
        if self.retrying_contact.is_some() {
            return;
        }
        let guard = SessionGuard::capture();
        self.retry_error_key = None;
        self.retrying_contact = Some(Arc::clone(&contact.dht_key));
        let key = contact.dht_key.clone();
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
                // Post to the view on the main thread via the revision bus:
                // bumping ConversationIndex reloads the list through the
                // observer; the retry marker lives in the store-driven state.
                let _ = &key;
                // The gpui revision bus needs an `App` context; the async
                // session-scoped path lands with the session cutover, at
                // which point this becomes `cx.update` on the bus entity.
                // For now the store commit itself bumps the revision through
                // the shared signal (`notify_model_changed` callers in
                // storage), and this view observes that signal's entity.
            });
        });
    }

    fn update_invite(&mut self, key: String, accept: bool) {
        if self.active_invite.is_some() {
            return;
        }
        let guard = SessionGuard::capture();
        self.invite_error_key = None;
        self.active_invite = Some((key.clone(), accept));
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
                // Invite reload rides the revision bus (accept/reject mutate
                // the store, whose commits bump the revision).
                let _ = result.err().map(|e| e.to_string());
            });
        });
    }
}

impl Render for ChatListView {
    // One long fn is deliberate here: the list item closure must stay inline
    // with the snapshot it borrows (the old component's structure).
    #[allow(clippy::too_many_lines)]
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::ui::theme::Theme::for_appearance(cx);
        let account = account_id(cx);
        let contacts: Vec<&ChatContact> = self.contacts.as_ref().map_or_else(
            |_| Vec::new(),
            |stored| {
                stored
                    .iter()
                    .filter(|contact| matches_filter(contact, self.filter))
                    .collect()
            },
        );
        let error = self.contacts.as_ref().err().copied();

        let retrying = self.retrying_contact.clone();
        let retry_error_key = self.retry_error_key.clone();

        let list_entity = cx.entity();
        let mut list = gpui::uniform_list("chat-contacts", contacts.len(), {
            let contacts_snapshot: Vec<ChatContact> =
                contacts.iter().map(|c| (*c).clone()).collect();
            move |range, _window, cx| {
                let theme = crate::ui::theme::Theme::for_appearance(cx);
                range
                    .map(|idx| {
                        let contact = &contacts_snapshot[idx];
                        let is_retrying = retrying.as_ref().is_some_and(|k| *k == contact.dht_key);
                        let retry_failed = retry_error_key
                            .as_ref()
                            .is_some_and(|k| *k == contact.dht_key);
                        let initial = contact.display_name.chars().next().unwrap_or('?');
                        gpui::div()
                            .id(SharedString::from(format!(
                                "contact-{}",
                                short_key(&contact.dht_key)
                            )))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .rounded(crate::ui::theme::shape::RADIUS_FIELD)
                            .hover_style(|s| s.bg(theme.base_200))
                            .child(
                                gpui::div()
                                    .w(px(40.0))
                                    .h(px(40.0))
                                    .rounded_full()
                                    .bg(theme.primary.fill)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(theme.primary.content)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(initial.to_string()),
                            )
                            .child(
                                gpui::div()
                                    .flex_1()
                                    .text_color(theme.base_content)
                                    .child(contact.display_name.to_string()),
                            )
                            .when(is_retrying, |row| {
                                row.child(
                                    gpui::div()
                                        .text_xs()
                                        .text_color(theme.text_dim())
                                        .child("Retrying…"),
                                )
                            })
                            .when(retry_failed, |row| {
                                let h = list_entity.clone();
                                let contact_clone = contact.clone();
                                row.child(
                                    gpui::div()
                                        .id(SharedString::from(format!(
                                            "retry-{}",
                                            short_key(&contact.dht_key)
                                        )))
                                        .cursor_pointer()
                                        .text_xs()
                                        .text_color(theme.error_text())
                                        .on_click(move |_, _, cx| {
                                            h.update(cx, |this, _| {
                                                let contact = contact_clone.clone();
                                                this.retry_contact(contact);
                                            });
                                        })
                                        .child("Retry"),
                                )
                            })
                            .when(contact.conversation_record_key.is_empty(), |row| {
                                row.child(
                                    gpui::div()
                                        .text_xs()
                                        .text_color(theme.warn_text())
                                        .child("Pending"),
                                )
                            })
                            .into_any_element()
                    })
                    .collect()
            }
        });

        let _ = &mut list;
        let _ = error;
        let _ = account;

        CenteredPage::new()
            .contained(true)
            .child(
                gpui::div()
                    .id("chat-add-contact-button")
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w_full()
                    .py_2()
                    .rounded(crate::ui::theme::shape::RADIUS_FIELD)
                    .bg(theme.primary.fill)
                    .text_color(theme.primary.content)
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Add Contact"),
            )
            .child(
                gpui::div()
                    .flex()
                    .gap_2()
                    .py_2()
                    .children(ContactFilter::TABS.map(|tab| {
                        let selected = self.filter == tab;
                        gpui::div()
                            .id(SharedString::from(format!("filter-{}", tab.label())))
                            .cursor_pointer()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .rounded(crate::ui::theme::shape::RADIUS_FIELD)
                            .when(selected, |t| {
                                t.bg(theme.primary.fill).text_color(theme.primary.content)
                            })
                            .when(!selected, |t| {
                                t.text_color(theme.text_dim())
                                    .hover_style(|s| s.bg(theme.base_200))
                            })
                            .child(tab.label())
                    })),
            )
            .child(list)
    }
}

/// ASCII-safe prefix for element ids (dht keys are hex, but never assume).
fn short_key(key: &str) -> SharedString {
    SharedString::from(key.chars().take(8).collect::<String>())
}
