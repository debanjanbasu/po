//! Account page — profile overview, settings sections, logout, and account deletion.

use crate::{
    components::{
        settings::shell::{SettingsCard, SettingsPage},
        shared::{
            FeedbackKind, FormFeedback, ModalDialog, ThemeDropdown, close_modal_dialog,
            use_modal_dialog_id,
        },
    },
    nonfatal,
    router::Route,
    session::epoch::SessionGuard,
    session::phase::SESSION,
    user::{biometric, delete_current_account, log_out},
};
use dioxus::prelude::*;

// Shared about content — generated at build time from `shared/about.json`.
mod about {
    include!(concat!(env!("OUT_DIR"), "/about_generated.rs"));
}
/// Account & settings page.
#[component]
pub fn Account() -> Element {
    let user_info = use_memo(|| {
        SESSION
            .read()
            .authenticated_user()
            .map(|u| (u.username_arc(), u.dht_key_arc()))
    });
    let Some((username, dht_key)) = user_info() else {
        return rsx! { div { class: "flex items-center justify-center h-full", "Redirecting…" } };
    };
    let delete_dialog_id = use_modal_dialog_id("delete-account-dialog");
    let cleanup_dialog_id = use_modal_dialog_id("cleanup-warning-dialog");
    let mut logging_out = use_signal(|| false);
    let mut logout_error = use_signal(|| None::<String>);
    let mut deleting = use_signal(|| false);
    let mut delete_modal_open = use_signal(|| false);
    let mut cleanup_warning = use_signal(|| false);
    let busy = logging_out() || deleting();
    let mut close_delete_modal = move || delete_modal_open.set(false);

    rsx! {
        SettingsPage {
            // ── Profile header ──
            SettingsCard {
                li { class: "list-row items-center",
                    div { class: "avatar avatar-placeholder",
                        div { class: "bg-neutral text-neutral-content w-10 rounded-full",
                            span { "{username.chars().next().unwrap_or('?').to_uppercase()}" }
                        }
                    }
                    div {
                        div { "{username}" }
                        div { class: "truncate max-w-56", "{dht_key}" }
                    }
                }
            }

            // ── Settings sections ──
            SettingsCard {
                Link { to: Route::Profile, class: "list-row",
                    span {
                        class: "icon-[line-md--person-twotone]",
                        "aria-hidden": "true",
                    }
                    div {
                        div { "Profile" }
                        div { "Date of Birth" }
                    }
                }
                Link { to: Route::Security, class: "list-row",
                    span {
                        class: "icon-[line-md--security-twotone]",
                        "aria-hidden": "true",
                    }
                    div {
                        div { "Security" }
                        div {
                            if biometric::can_check() {
                                "PIN · Password · Biometrics"
                            } else {
                                "PIN · Password"
                            }
                        }
                    }
                }
                Link { to: Route::Snapshots, class: "list-row",
                    span {
                        class: "icon-[line-md--cloud-down-twotone]",
                        "aria-hidden": "true",
                    }
                    div {
                        div { "Data & Backups" }
                        div { "Export · Restore · Snapshots" }
                    }
                }
                Link { to: Route::Pairing, class: "list-row",
                    span {
                        class: "icon-[line-md--laptop-twotone]",
                        "aria-hidden": "true",
                    }
                    div {
                        div { "Pair New Device" }
                        div { "Link another device to this identity" }
                    }
                }
                li { class: "list-row items-center",
                    span {
                        class: "icon-[line-md--paint-drop-twotone]",
                        "aria-hidden": "true",
                    }
                    div { class: "flex-1",
                        div { "Appearance" }
                        div { "Theme" }
                    }
                    ThemeDropdown {}
                }
            }

            // ── About (collapsible) ──
            details { class: "collapse collapse-arrow bg-base-200 rounded-box shadow-md",
                summary { class: "collapse-title flex items-center gap-3",
                    span {
                        class: "icon-[line-md--question-circle-twotone]",
                        "aria-hidden": "true",
                    }
                    div {
                        div { "About" }
                        div { "v{env!(\"CARGO_PKG_VERSION\")}" }
                    }
                }
                div { class: "collapse-content text-base-content/70",
                    p { class: "text-base-content mb-2", "po — {about::TAGLINE}" }
                    ul { class: "space-y-2",
                        {about::FEATURES.iter().map(|f| rsx! {
                            li { class: "flex items-start gap-2",
                                span { class: "{f.icon} shrink-0 mt-0.5" }
                                "{f.short}"
                            }
                        })}
                    }
                    div { class: "flex flex-wrap gap-2 pt-3",
                        {about::LINKS.iter().map(|&(href, label)| rsx! {
                            a {
                                class: "btn btn-sm btn-ghost",
                                href,
                                target: "_blank",
                                span { class: "icon-[line-md--external-link-rounded]" }
                                "{label}"
                            }
                        })}
                    }
                }
            }

            // ── Logout ──
            if let Some(error) = logout_error() {
                FormFeedback { kind: FeedbackKind::Error, message: error, class: "text-xs" }
            }
            button {
                id: "account-logout-button",
                class: "btn btn-sm btn-error btn-block",
                disabled: busy,
                onclick: move |_| async move {
                    logging_out.set(true);
                    logout_error.set(None);
                    if let Err(error) = log_out().await {
                        logout_error.set(Some(format!("Unable to log out: {error}")));
                    }
                    logging_out.set(false);
                },
                span { class: "icon-[line-md--log-out]", "aria-hidden": "true" }
                "Log out"
            }

            // ── Danger zone: Delete account ──
            details { class: "collapse collapse-arrow border border-error/30 bg-base-200 rounded-box",
                summary { class: "collapse-title text-error", "Danger Zone" }
                div { class: "collapse-content space-y-3",
                    p { class: "text-base-content/70",
                        "Permanently delete your account and all local device data. "
                        "This cannot be undone."
                    }
                    button {
                        class: "btn btn-sm btn-error btn-outline btn-block",
                        disabled: busy,
                        onclick: move |_| delete_modal_open.set(true),
                        span {
                            class: "icon-[line-md--close-circle-twotone]",
                            "aria-hidden": "true",
                        }
                        "Delete Account"
                    }
                }
            }
        }

        if delete_modal_open() {
            ModalDialog {
                dialog_id: delete_dialog_id.clone(),
                title: "Delete your account?",
                title_class: "text-error",
                close_disabled: deleting(),
                on_close: move |()| close_delete_modal(),

                p { class: "py-1",
                    "All local data will be permanently erased. "
                    "Your identity on the network will remain but you will lose access to it from this device."
                }
                div { class: "modal-action flex gap-2",
                    button {
                        class: "btn btn-sm",
                        disabled: deleting(),
                        autofocus: true,
                        onclick: {
                            let dialog_id = delete_dialog_id.clone();
                            move |_| {
                                close_modal_dialog(&dialog_id);
                                close_delete_modal();
                            }
                        },
                        "Cancel"
                    }
                    button {
                        r#type: "button",
                        class: "btn btn-sm btn-error",
                        disabled: deleting(),
                        onclick: move |_| {
                            let dialog_id = delete_dialog_id.clone();
                            async move {
                                let guard = SessionGuard::capture();
                                close_modal_dialog(&dialog_id);
                                close_delete_modal();
                                deleting.set(true);
                                let failed = delete_current_account().await.is_err();
                                if failed && guard.is_current() {
                                    deleting.set(false);
                                    cleanup_warning.set(true);
                                }
                            }
                        },
                        "Delete permanently"
                    }
                }
            }
        }
        if cleanup_warning() {
            ModalDialog {
                dialog_id: cleanup_dialog_id,
                title: "Local cleanup incomplete",
                title_class: "text-warning",
                close_disabled: true,
                on_close: move |()| {},

                p { class: "py-1",
                    "Some local data could not be removed; it will be retried automatically."
                }
                div { class: "modal-action",
                    button {
                        class: "btn btn-sm btn-warning",
                        onclick: move |_| async move {
                            let guard = SessionGuard::capture();
                            cleanup_warning.set(false);
                            if guard.is_current() {
                                nonfatal!(
                                    "settings.account",
                                    "Failed to log out after account deletion",
                                    log_out().await
                                );
                            }
                        },
                        "Continue"
                    }
                }
            }
        }
    }
}
