//! Persistent biometric admission gate rendered for the pending session phase.

use crate::{
    components::shared::{CenteredPage, Spinner, SpinnerSize},
    session::phase::{
        SESSION, SessionPhase, abandon_second_factor, complete_admission, enter_second_factor,
    },
    user::biometric,
};
use dioxus::prelude::*;

#[component]
pub fn BiometricGate() -> Element {
    let mut attempt = use_signal(|| 0_u64);
    let mut failed = use_signal(|| false);

    use_effect(move || {
        let _ = attempt();
        let user = match &*SESSION.read() {
            SessionPhase::PendingBiometric(user) => user.clone(),
            _ => return,
        };
        failed.set(false);
        spawn(async move {
            if biometric::authenticate_async("Verify your identity").await {
                let is_same_admission = matches!(
                    &*SESSION.peek(),
                    SessionPhase::PendingBiometric(pending)
                        if pending.account_id() == user.account_id()
                );
                if is_same_admission && complete_admission(user).await.is_err() {
                    failed.set(true);
                }
            } else if matches!(*SESSION.peek(), SessionPhase::PendingBiometric(_)) {
                failed.set(true);
            }
        });
    });

    let return_to_login = move |_| {
        abandon_second_factor();
    };

    let use_pin = move |_| {
        if let Some(user) = SESSION.peek().pending_user().cloned() {
            enter_second_factor(user);
        }
    };

    rsx! {
        CenteredPage {
            div { class: "w-full max-w-xs mx-auto space-y-3 text-center",
                if failed() {
                    h1 { class: "text-lg font-semibold", "Biometric failed" }
                    p { class: "text-sm text-base-content/70",
                        "Authentication was cancelled or could not be completed."
                    }
                    button {
                        class: "btn btn-primary btn-sm btn-block",
                        onclick: move |_| attempt += 1,
                        "Retry"
                    }
                    button { class: "btn btn-sm btn-block", onclick: use_pin, "Use PIN/password" }
                    button { class: "btn btn-ghost btn-sm btn-block", onclick: return_to_login, "Different account" }
                } else {
                    Spinner { size: SpinnerSize::Lg, label: "Checking biometrics" }
                    p { class: "text-sm", "Verify your identity" }
                }
            }
        }
    }
}
