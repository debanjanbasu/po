//! PIN verification gate. Shown after successful credential check when
//! biometric authentication is unavailable or not enabled.
//!
//! Brute-force tracking is delegated to [`user::verify_pin`] which persists
//! both failed-attempt counts and cooldown timing, so remounts/restarts cannot
//! bypass lockout.

use crate::{
    components::shared::{CenteredPage, use_form_field_manual},
    helpers::{async_utils::now_epoch_secs, spawn},
    session::{
        epoch as session,
        phase::{SESSION, abandon_second_factor, complete_admission},
    },
    user::{
        self, User,
        password::{PinStatus, PinVerifyResult, pin_status},
        validation::validate_pin_msg,
    },
};
use dioxus::{logger::tracing::warn, prelude::*};

use super::use_countdown;

fn cooldown_remaining(until: u64) -> u64 {
    until.saturating_sub(now_epoch_secs())
}

fn pending_user_matches(expected: &str) -> bool {
    SESSION
        .peek()
        .pending_user()
        .is_some_and(|user| user.username() == expected)
}

/// Unified error state for the PIN gate — replaces separate `error: bool` +
/// `internal_error: Option<String>` signals.
#[derive(Clone)]
enum PinError {
    Incorrect,
    /// Internal failure — deliberately indistinguishable from other
    /// internal errors in the UI; no payload is surfaced.
    Internal,
}

fn apply_pin_state(
    error: Option<PinError>,
    remaining: u64,
    clear_pin: bool,
    mut pin: Signal<String>,
    mut pin_error: Signal<Option<PinError>>,
    mut lock_remaining: Signal<u64>,
) {
    pin_error.set(error);
    lock_remaining.set(remaining);
    if clear_pin {
        pin.set(String::new());
    }
}

/// Prompts the user for their PIN before granting full access.
#[component]
pub fn PinGate() -> Element {
    let default_hint = validate_pin_msg("").unwrap_or_default();
    let mut pin_handle = use_form_field_manual("", true, validate_pin_msg);
    let mut pin_error = use_signal(|| None::<PinError>);
    let lock_remaining = use_countdown(0);
    let lock_expired = use_memo(move || lock_remaining() == 0);
    let mut verifying = use_signal(|| false);
    let pending_username = use_memo(|| SESSION.read().pending_user().map(User::username_arc));
    let username = pending_username().unwrap_or_default();

    use_effect(move || {
        let Some(name) = pending_username() else {
            return;
        };
        let guard = session::SessionGuard::capture();
        spawn(async move {
            let status = pin_status(name.as_ref()).await;
            if !guard.is_current() || !pending_user_matches(name.as_ref()) {
                return;
            }
            match status {
                Ok(PinStatus::Ready { .. }) => {
                    apply_pin_state(None, 0, false, pin_handle.value, pin_error, lock_remaining);
                }
                Ok(PinStatus::Cooldown { until, .. }) => {
                    apply_pin_state(
                        None,
                        cooldown_remaining(until),
                        true,
                        pin_handle.value,
                        pin_error,
                        lock_remaining,
                    );
                }
                Err(e) => {
                    warn!(
                        "Failed to load PIN attempt state for {}: {e:#}",
                        name.as_ref()
                    );
                    apply_pin_state(
                        Some(PinError::Internal),
                        0,
                        false,
                        pin_handle.value,
                        pin_error,
                        lock_remaining,
                    );
                }
            }
        });
    });

    let should_verify = use_memo(move || (pin_handle.is_valid)() && lock_expired());

    // Harness seam (no-op in release builds): an unattended device answers the
    // second factor from the environment instead of waiting for a human.
    use_effect(move || {
        if *pin_handle.is_dirty.peek() {
            return;
        }
        let Some(pin) = crate::config::e2e::account_pin() else {
            return;
        };
        pin_handle.value.set(pin);
        pin_handle.is_dirty.set(true);
    });

    use_effect(move || {
        if !should_verify() || verifying() {
            return;
        }
        let Some(pending) = SESSION.peek().pending_user().cloned() else {
            return;
        };
        let name = pending.username().to_string();
        let guard = session::SessionGuard::capture();
        let attempt = (pin_handle.value)();
        pin_error.set(None);
        verifying.set(true);
        spawn(async move {
            let result = user::verify_pin(&name, &attempt).await;
            if !guard.is_current()
                || (pin_handle.value)().as_str() != attempt
                || !pending_user_matches(&name)
            {
                verifying.set(false);
                return;
            }
            match result {
                Ok(PinVerifyResult::Verified) => {
                    if let Err(error) = Box::pin(complete_admission(pending)).await {
                        warn!("Failed to finalize admission after PIN verification: {error}");
                        apply_pin_state(
                            Some(PinError::Internal),
                            0,
                            true,
                            pin_handle.value,
                            pin_error,
                            lock_remaining,
                        );
                    }
                }
                Ok(PinVerifyResult::Incorrect { .. }) => {
                    apply_pin_state(
                        Some(PinError::Incorrect),
                        0,
                        true,
                        pin_handle.value,
                        pin_error,
                        lock_remaining,
                    );
                }
                Ok(PinVerifyResult::Cooldown { until, .. }) => {
                    apply_pin_state(
                        None,
                        cooldown_remaining(until),
                        true,
                        pin_handle.value,
                        pin_error,
                        lock_remaining,
                    );
                }
                Err(e) => {
                    warn!("Failed to verify PIN for {name}: {e:#}");
                    apply_pin_state(
                        Some(PinError::Internal),
                        0,
                        true,
                        pin_handle.value,
                        pin_error,
                        lock_remaining,
                    );
                }
            }
            verifying.set(false);
        });
    });

    let is_locked = !lock_expired();
    let is_incorrect = matches!(pin_error(), Some(PinError::Incorrect));
    // The hint paragraph is `display: none` unless something marks the field
    // invalid (`overrides.css`), and the input's own `pattern` only constrains
    // the digit shape — so a well-formed wrong PIN, a cooldown and an internal
    // failure all need this class or their message is never painted.
    let is_internal = matches!(pin_error(), Some(PinError::Internal));
    rsx! {
        CenteredPage {
            fieldset {
                class: "fieldset w-full max-w-xs mx-auto gap-2",
                p { class: "text-center mb-2", "{username}" }
                div {
                    label {
                        class: "input input-sm validator w-full",
                        class: if is_incorrect || is_locked || is_internal { "validator-error" },
                        span {
                            class: "icon-[line-md--hash-small]",
                            "aria-hidden": "true",
                        }
                        input {
                            r#type: "password",
                            id: "pin-gate-input",
                            inputmode: "numeric",
                            "aria-label": "PIN",
                            placeholder: "PIN",
                            value: "{pin_handle.value}",
                            required: true,
                            disabled: is_locked,
                            pattern: "[0-9]{{6,}}",
                            autofocus: true,
                            oninput: move |ev: FormEvent| {
                                pin_error.set(None);
                                pin_handle.value.set(ev.value());
                                pin_handle.is_dirty.set(true);
                            },
                        }
                    }
                    p { class: "validator-hint",
                        if is_internal {
                            "Could not verify the PIN right now. Please try again."
                        } else if is_locked {
                            "Too many attempts. Try again in {lock_remaining}s"
                        } else if is_incorrect {
                            "Incorrect PIN"
                        } else {
                            "{default_hint}"
                        }
                    }
                }
                button {
                    class: "btn btn-sm btn-ghost btn-block",
                    onclick: move |_| {
                        abandon_second_factor();
                    },
                    "Use a different account"
                }
            }
        }
    }
}
