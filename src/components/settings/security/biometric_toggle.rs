//! Biometric authentication toggle sub-component.

use super::use_account_epoch_reset;
use crate::{
    components::{
        settings::shell::SettingsCard,
        shared::{FeedbackKind, FormFeedback},
    },
    helpers::spawn,
    session::epoch::SessionGuard,
    storage::background_errors::report_nonfatal,
    user::{self, SecurityUpdateError, biometric, ctx::UserCtx},
};
use dioxus::prelude::*;

#[component]
pub(super) fn BiometricToggle(username: ReadSignal<String>, bio_enabled: Memo<bool>) -> Element {
    let mut feedback = use_signal(|| None::<(FeedbackKind, &'static str)>);
    let mut prompt_in_flight = use_signal(|| false);
    use_account_epoch_reset(move || feedback.set(None));

    rsx! {
        SettingsCard {
            li { class: "list-row",
                div { class: "list-col-grow",
                    div { "Biometric Authentication" }
                    div { "Use biometrics for quick access" }
                    if prompt_in_flight() {
                        p { class: "text-base-content/60 mt-1", role: "status",
                            "Updating biometrics..."
                        }
                    } else if let Some((kind, message)) = feedback() {
                        FormFeedback {
                            kind,
                            message: message.to_owned(),
                            compact: true,
                            class: "mt-1",
                        }
                    }
                }
                button {
                    r#type: "button",
                    role: "switch",
                    class: "toggle toggle-primary",
                    aria_label: "Biometric authentication",
                    aria_checked: if bio_enabled() { "true" } else { "false" },
                    disabled: prompt_in_flight(),
                    onclick: move |_| {
                        if prompt_in_flight() {
                            return;
                        }
                        let desired = !bio_enabled();
                        let name = username();
                        let Some(ctx) = UserCtx::try_current()
                            .filter(|ctx| ctx.username.as_ref() == name)
                        else {
                            feedback.set(Some((
                                FeedbackKind::Error,
                                "Sign in again before changing biometrics.",
                            )));
                            return;
                        };
                        let guard = SessionGuard::capture();
                        feedback.set(None);
                        prompt_in_flight.set(true);
                        spawn(async move {
                            let prompt = if desired {
                                "Confirm to enable biometrics"
                            } else {
                                "Confirm to disable biometrics"
                            };
                            let confirmed = biometric::authenticate_async(prompt).await;
                            if !guard.is_current() {
                                prompt_in_flight.set(false);
                                return;
                            }
                            if !confirmed {
                                prompt_in_flight.set(false);
                                feedback.set(Some((
                                    FeedbackKind::Success,
                                    "Not confirmed. Nothing changed.",
                                )));
                                return;
                            }
                            let result = user::update_security(&ctx, None, Some(desired)).await;
                            prompt_in_flight.set(false);
                            if !guard.is_current() {
                                return;
                            }
                            match result {
                                Ok(_) => feedback.set(Some((
                                    FeedbackKind::Success,
                                    "Saved on this device.",
                                ))),
                                Err(SecurityUpdateError::SyncPending(error)) => {
                                    report_nonfatal(
                                        "settings.biometric_toggle",
                                        "Biometrics updated locally; synchronization could not be queued",
                                        error,
                                    );
                                    feedback.set(Some((
                                        FeedbackKind::Success,
                                        "Saved locally. Sync needs attention in Activity.",
                                    )));
                                }
                                Err(SecurityUpdateError::Local(error)) => {
                                    report_nonfatal(
                                        "settings.biometric_toggle",
                                        "Failed to update biometric setting",
                                        format!("{error:#}"),
                                    );
                                    feedback.set(Some((
                                        FeedbackKind::Error,
                                        "Unable to update biometrics. Please try again.",
                                    )));
                                }
                            }
                        });
                    },
                }
            }
        }
    }
}
