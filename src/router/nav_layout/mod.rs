//! Main app navigation layout with a bottom dock and drag-and-drop reorder.

use dioxus::prelude::*;

use crate::{
    chat::{
        hooks::send_from_deep_link,
        session_flow::{ChatInputError, add_contact},
    },
    components::{
        ErrorToast,
        overlays::{
            TOP_STRIP_PADDING,
            deep_link_toast::{DeepLinkToast, show_deep_link_notice, show_deep_link_toast},
        },
    },
    global_state::last_active_route,
    helpers::spawn,
    platform::{deep_link, share_urls},
    router::{
        Route,
        helpers::{mark_route_restored, route_is_restored, use_persist_app_state},
    },
    storage::background_errors::{push_background_error, report_nonfatal},
};

mod dock;

use dock::DockSection;

/// Root layout wrapping all authenticated routes with a responsive dock nav.
#[component]
pub fn NavLayout() -> Element {
    let nav = use_navigator();
    let current_route: Route = use_route();

    // Persist AppState (route, nav order, theme) reactively from this stable scope.
    use_persist_app_state();

    use_effect(move || {
        if route_is_restored() {
            return;
        }
        // A launch link owns navigation, but must not leave persistence gated.
        let route = if deep_link::has_pending_action() {
            current_route.clone()
        } else {
            last_active_route().with(|last| {
                last.parse::<Route>()
                    .ok()
                    .filter(|route| !matches!(route, Route::PageNotFound { .. }))
                    .unwrap_or_else(|| current_route.clone())
            })
        };
        mark_route_restored();
        if last_active_route().peek().as_str() != route.to_string() {
            *last_active_route().write() = route.to_string();
        }
        if route != current_route {
            nav.replace(route);
        }
    });

    let mut claimed_action = deep_link::use_claimed_action(deep_link::DeepLinkOwner::NavLayout);
    use_effect(move || {
        // Pairing links can only run on a signed-out device: discard the
        // action here (NavLayout implies an authenticated session) but tell
        // the user why nothing happened instead of silently dropping it.
        if deep_link::has_pending_pairing()
            && deep_link::take_pending_action(deep_link::DeepLinkOwner::Pairing).is_some()
        {
            show_deep_link_notice(
                "Pairing links only work on a device that isn't signed in. Sign out first, then open the link again."
                    .to_string(),
            );
        }
        let Some(action) = claimed_action() else {
            return;
        };
        claimed_action.set(None);
        if let deep_link::DeepLinkAction::SendMessage { contact, text } = action {
            // Harness-only (`config::e2e::enabled()` gates the link itself):
            // the send path a surface with no typing channel drives.
            spawn(async move {
                send_from_deep_link(contact.as_ref(), text).await;
            });
            return;
        }
        let deep_link::DeepLinkAction::AddContact { username, dht_key } = action else {
            // Pairing links are owned by the pairing screen and are never
            // claimed here.
            return;
        };
        spawn(async move {
            match add_contact(username.as_ref(), dht_key.as_ref()).await {
                Err(ChatInputError::ContactExists) => {
                    nav.replace(Route::Chat);
                }
                Err(e) => {
                    report_nonfatal(
                        "nav_layout.deep_link",
                        "Failed to add contact from deep link",
                        &e,
                    );
                    push_background_error(
                        "nav_layout.deep_link",
                        format!("Failed to add contact: {e}"),
                    );
                }
                Ok(()) => {
                    show_deep_link_toast(
                        format!("{} added", username.as_ref()),
                        Some(
                            share_urls::compact_identity(username.as_ref(), dht_key.as_ref())
                                .into(),
                        ),
                    );
                    nav.replace(Route::Chat);
                }
            }
        });
    });

    rsx! {
        DeepLinkToast {}
        div {
            class: "flex flex-col h-dvh overflow-hidden landscape:flex-row",
            DockSection {}
            div { class: "{TOP_STRIP_PADDING} nav-scroll flex-1 min-h-0 overflow-y-auto landscape:pl-16",
                main { class: "h-full min-h-full",
                    ErrorBoundary {
                        handle_error: move |error: ErrorContext| rsx! {
                            ErrorToast { error }
                        },
                        Outlet::<Route> {}
                    }
                }
            }
        }
    }
}
