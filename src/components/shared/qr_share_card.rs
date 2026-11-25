//! Shared QR display card with download and share actions.

use base64::engine::{Engine, general_purpose::STANDARD};
use bytes::Bytes;
use dioxus::prelude::*;

use crate::{
    components::shared::use_transient_message,
    config,
    helpers::spawn,
    platform::share::{self, ShareError},
    session::epoch::SessionGuard,
};

#[component]
pub fn QrShareCard(
    title: &'static str,
    description: &'static str,
    webp: Bytes,
    download_filename: &'static str,
    details: Element,
    copy_text: String,
    share_text: String,
) -> Element {
    let share_webp = webp.clone();
    // Timed feedback for the two actions. Separate transients — a Share
    // click must not freeze a pending "Copied ✓" reset, and vice versa.
    let (copy_feedback, mut copy_feedback_set) =
        use_transient_message(config::ui::SHARE_FEEDBACK_MS);
    let (share_notice, mut share_notice_set) = use_transient_message(config::ui::SHARE_NOTICE_MS);
    let mut action_error = use_signal(|| None::<ShareError>);
    let mut activity = use_signal(|| None::<&'static str>);
    let mut generation = use_signal(|| 0_u64);
    use_effect(use_reactive((&copy_text, &share_text, &webp), move |_| {
        let next = generation.peek().wrapping_add(1);
        generation.set(next);
        copy_feedback_set.clear();
        share_notice_set.clear();
        action_error.set(None);
    }));
    let data_url = use_memo(use_reactive((&webp,), |(webp,)| {
        format!("data:image/webp;base64,{}", STANDARD.encode(&webp))
    }));

    rsx! {
        div { class: "card bg-base-200 shadow-sm",
            div { class: "card-body p-4 gap-3",
                h3 { "{title}" }
                p { class: "text-base-content/60", "{description}" }

                if cfg!(target_arch = "wasm32") {
                    button {
                        class: "w-full max-w-40 mx-auto block rounded-box focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-secondary",
                        r#type: "button",
                        title: "Download QR image",
                        "aria-label": "Download QR image",
                        disabled: activity().is_some(),
                        onclick: move |_| {
                            if activity.peek().is_some() {
                                return;
                            }
                            activity.set(Some("Downloading..."));
                            action_error.set(None);
                            share_notice_set.clear();
                            let attempt = *generation.peek();
                            let guard = SessionGuard::capture();
                            let webp = webp.clone();
                            spawn(async move {
                                let result =
                                    share::download_bytes_async(webp.to_vec(), download_filename, "image/webp")
                                        .await;
                                guard.commit(|| {
                                    activity.set(None);
                                    if attempt != *generation.peek() {
                                        return;
                                    }
                                    match result {
                                        Ok(outcome) => share_notice_set.set(outcome.message()),
                                        Err(error) => action_error.set(Some(error)),
                                    }
                                });
                            });
                        },
                        img {
                            class: "w-full h-auto",
                            src: "{data_url}",
                            alt: "QR code",
                        }
                    }
                } else {
                    div { class: "w-full max-w-40 mx-auto",
                        img {
                            class: "w-full h-auto",
                            src: "{data_url}",
                            alt: "QR code",
                        }
                    }
                }

                {details}

                div { class: "flex justify-center gap-2",
                    button {
                        class: "btn btn-sm btn-ghost",
                        "aria-label": "Copy identity to clipboard",
                        disabled: activity().is_some(),
                        onclick: move |_| {
                            if activity.peek().is_some() {
                                return;
                            }
                            activity.set(Some("Copying..."));
                            action_error.set(None);
                            copy_feedback_set.clear();
                            share_notice_set.clear();
                            let attempt = *generation.peek();
                            let guard = SessionGuard::capture();
                            let text = copy_text.clone();
                            spawn(async move {
                                let result = share::copy_to_clipboard(&text).await;
                                guard.commit(|| {
                                    activity.set(None);
                                    if attempt != *generation.peek() {
                                        return;
                                    }
                                    match result {
                                        Ok(()) => copy_feedback_set.set("Copied ✓"),
                                        Err(error) => action_error.set(Some(error)),
                                    }
                                });
                            });
                        },
                        span { class: "icon-[heroicons--clipboard-document]" }
                        if copy_feedback().is_some() { "Copied ✓" } else { "Copy" }
                    }
                    button {
                        class: "btn btn-sm btn-secondary",
                        "aria-label": "Share invite link via another app",
                        disabled: activity().is_some(),
                        onclick: move |_| {
                            if activity.peek().is_some() {
                                return;
                            }
                            activity.set(Some("Sharing..."));
                            action_error.set(None);
                            share_notice_set.clear();
                            let attempt = *generation.peek();
                            let guard = SessionGuard::capture();
                            let text = share_text.clone();
                            let webp = share_webp.clone();
                            spawn(async move {
                                let result = share::share_text_and_image(&text, &webp).await;
                                guard.commit(|| {
                                    activity.set(None);
                                    if attempt != *generation.peek() {
                                        return;
                                    }
                                    match result {
                                        Ok(outcome) => share_notice_set.set(outcome.message()),
                                        Err(ShareError::Cancelled) => {
                                            share_notice_set.set(ShareError::Cancelled.to_string());
                                        }
                                        Err(error) => action_error.set(Some(error)),
                                    }
                                });
                            });
                        },
                        span { class: "icon-[heroicons--share]" }
                        "Share"
                    }
                }
                if let Some(message) = activity() {
                    p { class: "text-xs text-base-content/60 text-center", role: "status", "{message}" }
                } else if let Some(notice) = share_notice() {
                    p { class: "text-info text-center", role: "status", "{notice}" }
                }
                if let Some(error) = action_error() {
                    p {
                        class: if error == ShareError::Cancelled {
                            "text-base-content/60 text-center"
                        } else {
                            "text-error text-center"
                        },
                        role: if error == ShareError::Cancelled { "status" } else { "alert" },
                        "{error}"
                    }
                }
            }
        }
    }
}
