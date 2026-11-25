//! Authentication components: login, PIN gate, device pairing.

mod attempt_controller;
mod biometric_gate;
mod login_signup;
mod pairing;
mod pin_gate;

use dioxus::prelude::*;

use crate::{
    components::shared::{CenteredPage, Spinner, SpinnerSize},
    helpers::async_utils::sleep_ms,
    storage::background_errors::report_nonfatal,
    user::{User, admit_user, clear_pending_admission_effects},
};

pub use attempt_controller::{LatestAttemptController, use_latest_attempt_controller};
pub use biometric_gate::BiometricGate;
pub use login_signup::LoginSignup;
pub use pairing::Pairing;
pub use pin_gate::PinGate;

/// A simple countdown timer that decrements every second.
///
/// Returns a `Signal<u64>` with the remaining seconds.
/// When the countdown reaches zero, the signal stays at 0 and the timer pauses.
/// Setting the signal to a non-zero value resumes the countdown.
///
/// Uses 1-second polling rather than timer-based scheduling. This is intentional:
/// the hook is client-side only, fires at most once per second, and the polling
/// approach avoids platform-specific timer APIs while staying trivially
/// cancellable via the Dioxus future lifecycle.
pub fn use_countdown(initial_secs: u64) -> Signal<u64> {
    let mut remaining = use_signal(|| initial_secs);
    let active = use_memo(move || remaining() > 0);
    use_resource(move || {
        let active = active();
        async move {
            if !active {
                return;
            }
            loop {
                sleep_ms(1000).await;
                let current = *remaining.peek();
                if current == 0 {
                    break;
                }
                remaining.set(current.saturating_sub(1));
                if current <= 1 {
                    break;
                }
            }
        }
    });
    remaining
}

/// Neutral boot splash rendered while startup restores a remembered session.
///
/// Shown for the `SessionPhase::Restoring` phase so the login form never
/// flashes (and is never interactable) before local init decides whether a
/// remembered user will be admitted.
#[component]
pub fn RestoreSplash() -> Element {
    rsx! {
        CenteredPage {
            // The spinner is an inline-block, so `mx-auto` alone leaves it at
            // the left edge of the content column — the parent centres it.
            div { class: "text-center",
                Spinner { size: SpinnerSize::Lg, label: "Starting up" }
            }
        }
    }
}

/// Outcome of [`finalize_admission_ui`].
pub enum AdmissionUi {
    /// User admitted; the session signal takes over from here.
    Admitted,
    /// Attempt superseded — staged effects cleared; caller should bail out.
    Superseded,
    /// Admission failed (already reported); caller shows its own error UI.
    Failed,
}

/// Shared tail of every login/signup/pairing flow: bail out (clearing staged
/// admission effects) when the attempt was superseded, otherwise admit the
/// staged user and funnel failures through the nonfatal reporter.
pub async fn finalize_admission_ui(
    user: User,
    is_current: bool,
    domain: &'static str,
    context: &'static str,
) -> AdmissionUi {
    if !is_current {
        clear_pending_admission_effects();
        return AdmissionUi::Superseded;
    }
    match admit_user(user).await {
        Ok(()) => AdmissionUi::Admitted,
        Err(error) => {
            report_nonfatal(domain, context, error);
            AdmissionUi::Failed
        }
    }
}
