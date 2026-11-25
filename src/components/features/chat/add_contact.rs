//! Contact exchange dialog: share your QR and scan/paste others.

use dioxus::prelude::*;

use crate::{
    chat::session_flow::{ChatInputError, add_contact_from_input, parse_contact_input},
    components::shared::{
        ModalDialog, QrAcquireButtons, QrShareCard, Spinner, SpinnerSize, close_modal_dialog,
        use_modal_dialog_id,
    },
    config::e2e,
    helpers::spawn,
    platform::{
        qr::{self, generate_qr_webp},
        share_urls::{ShareBundle, compact_identity, contact_deep_link, contact_share_url},
    },
    session::phase::SESSION,
    storage::background_errors::report_nonfatal,
};

/// C-S1: maps a `ChatInputError` to a user-facing `&'static str` message.
const fn contact_input_error_message(err: &ChatInputError) -> &'static str {
    match err {
        ChatInputError::InvalidFormat
        | ChatInputError::InputTooLong
        | ChatInputError::InvalidUsername => {
            "Invalid contact format. Use username|dht_key or scan a valid QR code."
        }
        ChatInputError::InvalidDhtKey => "Invalid contact key format.",
        ChatInputError::ContactExists => "This contact is already in your list.",
        ChatInputError::SelfContact => "You cannot add yourself as a contact.",
        ChatInputError::NoIdentity => "Finish onboarding before adding contacts.",
        ChatInputError::NotLoggedIn => "Sign in again and retry adding the contact.",
        ChatInputError::SaveFailed(_) => "Unable to save this contact right now. Please try again.",
    }
}

/// Dialog for exchanging contacts via QR code or manual entry.
#[component]
pub fn AddContact(on_close: EventHandler<()>) -> Element {
    let dialog_id = use_modal_dialog_id("add-contact-dialog");
    let mut contact_input = use_signal(String::new);
    let mut scan_error = use_signal(|| None::<String>);
    let mut qr_error = use_signal(|| None::<String>);
    let mut adding = use_signal(|| false);
    let mut last_attempted_input = use_signal(|| None::<String>);

    let user_identity = use_memo(|| {
        SESSION
            .read()
            .authenticated_user()
            .map(|u| (u.username_arc(), u.dht_key_arc()))
    });

    // Generate shareable QR from current user identity reactively so if
    // CreateIdentity finishes while the modal is open, the QR code appears.
    let share_data = use_resource(move || {
        let identity = user_identity();
        async move {
            let (username_arc, dht_key) = identity?;
            if dht_key.is_empty() {
                return None;
            }
            let username = username_arc.to_string();
            let deep = contact_deep_link(&username, &dht_key);
            let compact = compact_identity(&username, &dht_key);
            let share_text = format!(
                "{}\n\n{}",
                ShareBundle {
                    title: Some("Add me on po".to_string()),
                    username: Some(username.clone()),
                    deep_link: Some(deep),
                    ..ShareBundle::default()
                }
                .to_multiline_text(),
                compact,
            );
            // QR encodes the https share URL: external camera apps open the
            // landing page (which hands off to the app) while in-app scanners
            // parse the same URL directly. Raw `username|dht_key` stays paste-only.
            let qr_link = contact_share_url(&username, &dht_key);
            match generate_qr_webp(qr_link.clone()).await {
                Ok(webp) => {
                    qr_error.set(None);
                    // Harness seam: this device's own share identity, so a
                    // tandem peer can build an po://chat link for it without
                    // introspecting the other instance. No secrets here.
                    e2e::event!("identity", username = %username, dht_key = %dht_key);
                    if e2e::enabled() {
                        // Prove the rendered QR actually encodes the contact URL.
                        qr::report_qr_round_trip(&webp, &qr_link).await;
                    }
                    Some((webp, username, compact, share_text))
                }
                Err(e) => {
                    qr_error.set(Some(format!("Failed to generate QR code: {e}")));
                    None
                }
            }
        }
    });

    let dialog_id_for_effect = dialog_id.clone();
    use_effect(move || {
        if adding() {
            return;
        }
        let trimmed = {
            let input = contact_input.read();
            let trimmed = input.trim();
            if trimmed.is_empty() || parse_contact_input(trimmed).is_err() {
                return;
            }
            if last_attempted_input().as_deref() == Some(trimmed) {
                return;
            }
            trimmed.to_string()
        };
        last_attempted_input.set(Some(trimmed.clone()));
        adding.set(true);
        scan_error.set(None);
        let dialog_id = dialog_id_for_effect.clone();
        spawn(async move {
            let result = add_contact_from_input(&trimmed).await;
            adding.set(false);
            match result {
                Ok(()) => {
                    close_modal_dialog(&dialog_id);
                    on_close.call(());
                }
                Err(e) => {
                    report_nonfatal("components.chat.add_contact", "Failed to add contact", &e);
                    scan_error.set(Some(contact_input_error_message(&e).to_string()));
                }
            }
        });
    });
    rsx! {
        ModalDialog {
            dialog_id,
            title: "Add Contact",
            panel_class: "w-full max-w-sm max-h-[calc(100dvh-2rem)] overflow-y-auto",
            show_close_button: true,
            close_button_label: "Close add contact dialog",
            on_close: move |()| on_close.call(()),

            div { class: "space-y-4",
                if let Some(err) = qr_error() {
                    div { class: "text-error text-center", "{err}" }
                }
                if let Some(Some((webp, username, copy_text, share_text))) = share_data.read().as_ref() {
                    div { class: "space-y-4",
                        QrShareCard {
                            title: "Your Contact QR",
                            description: "Share this QR code or link so others can add you as a contact.",
                            webp: webp.clone(),
                            download_filename: "po-contact.webp",
                            copy_text: copy_text.clone(),
                            share_text: share_text.clone(),
                            details: rsx! {
                                p { class: "text-center mt-2", "@{username}" }
                            },
                        }
                        div { class: "divider", "OR" }
                    }
                }

                div { class: "space-y-2",
                    h3 { "Add a Contact" }
                    div { class: "join w-full",
                        input {
                            r#type: "text",
                            "aria-label": "Contact details",
                            class: "input input-sm join-item w-full",
                            placeholder: "Username, identity link (po://contact?…), or shared text",
                            value: contact_input,
                            autofocus: true,
                            disabled: adding(),
                            oninput: move |ev| {
                                let next = ev.value();
                                if last_attempted_input.peek().as_deref() != Some(next.trim()) {
                                    last_attempted_input.set(None);
                                }
                                scan_error.set(None);
                                contact_input.set(next);
                            },
                        }
                        QrAcquireButtons {
                            disabled: adding(),
                            on_result: move |result: String| {
                                if *adding.peek() {
                                    return;
                                }
                                let trimmed = result.trim();
                                if last_attempted_input.peek().as_deref() != Some(trimmed) {
                                    last_attempted_input.set(None);
                                }
                                contact_input.set(result.clone());
                                if let Err(e) = parse_contact_input(trimmed) {
                                    scan_error.set(Some(
                                        contact_input_error_message(&e).to_string(),
                                    ));
                                } else {
                                    scan_error.set(None);
                                }
                            },
                            on_error: move |msg: String| scan_error.set(Some(msg)),
                            scan_aria_label: "Scan contact QR",
                            upload_aria_label: "Upload contact QR",
                        }
                    }
                    if let Some(err) = scan_error() {
                        div { class: "text-error mt-1", "{err}" }
                    }
                    p { class: "text-base-content/60 mt-1",
                        "Scan a contact's QR code or paste their username|dht_key"
                    }
                }

                if adding() {
                    div {
                        class: "flex items-center justify-center gap-2 text-xs text-base-content/70",
                        role: "status",
                        Spinner { size: SpinnerSize::Xs }
                        span { "Adding contact…" }
                    }
                }
            }
        }
    }
}
