//! `PairModeSection` — new-device side of pairing: submit PIN + account password
//! for admission, with inline progress and error feedback.

use std::borrow::Cow;

use dioxus::{logger::tracing::warn, prelude::*};

use crate::{
    components::{
        auth::{AdmissionUi, finalize_admission_ui, use_countdown, use_latest_attempt_controller},
        shared::{
            AutoSubmitGate, QrAcquireButtons, RetryButton, Spinner, SpinnerSize, apply_form_input,
            use_form_field_manual,
        },
    },
    config::{e2e, validation::PAIRING_PIN_EXACT_LEN},
    helpers::{async_utils::sleep_ms, spawn},
    platform::deep_link,
    storage::{
        background_errors::report_nonfatal,
        pairing_sync_status::{self, PAIRING_SYNC_STATUS},
    },
    user::{
        clear_pending_admission_effects, stage_device_login,
        validation::{validate_new_password_msg, validate_pairing_payload, validate_pairing_pin},
    },
};

use super::actions::{
    PairAttempt, PreparedPairing, normalize_pairing_input, pair_device, pair_error_message,
    reset_pair_state,
};

/// Harness retry budget for an unattended joiner: the anchor's identity
/// record may not be replicated to the network yet when the joiner starts.
const E2E_PAIR_RETRIES: u32 = 6;

/// Wait before an unattended joiner re-fires a failed attempt: no
/// app-observable event proves the anchor's identity record has replicated.
const E2E_PAIR_RETRY_WAIT_MS: u64 = 5_000;

#[component]
pub(super) fn PairModeSection() -> Element {
    let mut payload = use_form_field_manual("", false, |v| {
        if validate_pairing_payload(v) {
            None
        } else {
            Some(
                "Invalid pairing data format. Must be a valid po:// link, po-ai.pages.dev link, or base64-encoded payload.",
            )
        }
    });
    let mut pin = use_form_field_manual("", false, |v| {
        if validate_pairing_pin(v) {
            None
        } else {
            Some("PIN must contain the required number of digits")
        }
    });
    let mut password = use_form_field_manual("", false, validate_new_password_msg);
    let mut remember_me = use_signal(|| true);
    let mut scan_error = use_signal(|| None::<Cow<'static, str>>);
    let mut pair_error = use_signal(|| None::<Cow<'static, str>>);
    let pair_gate = AutoSubmitGate::<(String, String, String)>::use_gate();
    let mut cooldown_remaining = use_countdown(0);
    let cooldown_expired = use_memo(move || cooldown_remaining() == 0);
    let pair_attempt = use_latest_attempt_controller();
    let mut claimed_action = deep_link::use_claimed_action(deep_link::DeepLinkOwner::Pairing);
    // Harness retry budget for an unattended joiner (see the failure branch).
    let mut e2e_retries = use_signal(|| 0_u32);

    let pair_valid =
        use_memo(move || (payload.is_valid)() && (pin.is_valid)() && (password.is_valid)());

    // Harness seam (no-op in release builds): an unattended joiner takes its
    // payload, PIN and password from the environment and then rides the
    // ordinary auto-submit below — same validation, same admission path,
    // values never logged. See docs/device-testing.md.
    use_effect(move || {
        if !(payload.value)().is_empty()
            || !(pin.value)().is_empty()
            || !(password.value)().is_empty()
        {
            return;
        }
        let Some((prefill_pin, prefill_password)) = e2e::pairing_credentials() else {
            return;
        };
        if let Some(prefill_payload) = e2e::pairing_payload() {
            let normalized = normalize_pairing_input(&prefill_payload);
            payload.value.set(normalized);
            payload.is_dirty.set(true);
        }
        pin.value.set(prefill_pin);
        pin.is_dirty.set(true);
        password.value.set(prefill_password);
        password.is_dirty.set(true);
    });

    use_effect(move || {
        if !pair_valid() || !cooldown_expired() || pair_attempt.is_busy() {
            return;
        }
        let inputs = ((payload.value)(), (pin.value)(), (password.value)());
        if !pair_gate.claim(inputs.clone()) {
            return;
        }
        let attempt = PairAttempt {
            payload: inputs.0,
            pin: inputs.1,
            password: inputs.2,
            remember_me: *remember_me.peek(),
        };
        if pair_error.peek().is_some() {
            pair_error.set(None);
        }
        clear_pending_admission_effects();
        let attempt_id = pair_attempt.begin();
        pair_attempt.replace_task(spawn(async move {
            let is_current = || {
                pair_attempt.matches(attempt_id)
                    && attempt.payload == payload.value.peek().as_str()
                    && attempt.pin == pin.value.peek().as_str()
                    && attempt.password == password.value.peek().as_str()
                    && attempt.remember_me == *remember_me.peek()
            };
            let result = pair_device(&attempt).await;
            if !is_current() {
                return;
            }
            match result {
                Ok(PreparedPairing {
                    user,
                    dht_key,
                    app_state,
                    identity_docs,
                    run,
                }) => {
                    let mut user = user;
                    stage_device_login(
                        &mut user,
                        dht_key,
                        app_state,
                        identity_docs,
                        attempt.remember_me,
                        run,
                    );
                    match finalize_admission_ui(
                        user,
                        is_current(),
                        "auth.pairing",
                        "Failed to finalize paired account",
                    )
                    .await
                    {
                        AdmissionUi::Admitted => {}
                        AdmissionUi::Superseded => {
                            pairing_sync_status::clear(run.status_run);
                            return;
                        }
                        AdmissionUi::Failed => {
                            pairing_sync_status::clear(run.status_run);
                            pair_error.set(Some(
                                "Pairing could not be saved on this device. Please try again."
                                    .into(),
                            ));
                        }
                    }
                }
                Err(e) => {
                    report_nonfatal("auth.pairing", "Pairing failed", &e);
                    if is_current() {
                        if let Some(secs) = e.cooldown_remaining_secs() {
                            cooldown_remaining.set(secs);
                            pair_error.set(None);
                        } else {
                            pair_error.set(Some(pair_error_message(&e)));
                        }
                        // Harness seam (no-op in release builds): a device with
                        // no typing channel has no way to press Retry, so an
                        // unattended joiner retries the same attempt — the
                        // anchor's record may not be replicated yet. Bounded,
                        // because the attempt is already known to have failed.
                        if e2e::enabled() && *e2e_retries.peek() < E2E_PAIR_RETRIES {
                            *e2e_retries.write() += 1;
                            spawn(async move {
                                sleep_ms(E2E_PAIR_RETRY_WAIT_MS).await;
                                let unchanged =
                                    pair_gate.peek().is_some_and(|(p, cur_pin, cur_pw)| {
                                        p == attempt.payload
                                            && cur_pin == attempt.pin
                                            && cur_pw == attempt.password
                                    });
                                if unchanged {
                                    pair_gate.release(Some(&(
                                        attempt.payload.clone(),
                                        attempt.pin.clone(),
                                        attempt.password.clone(),
                                    )));
                                }
                            });
                        }
                    }
                }
            }
            pair_attempt.finish(attempt_id);
        }));
    });
    let mut apply_payload = move |raw: &str| {
        cooldown_remaining.set(0);
        let normalized = normalize_pairing_input(raw);
        // Re-applying the SAME payload must not cancel a live pairing attempt:
        // the deep link and the launch environment often carry the same link,
        // and the link arriving after the form auto-submitted would otherwise
        // abort a pairing that is already in flight.
        let unchanged = payload.value.peek().as_str() == normalized;
        if !unchanged {
            reset_pair_state(scan_error, pair_error, pair_attempt);
        }
        if pair_gate.peek().is_some_and(|(p, _, _)| p != normalized) {
            pair_gate.clear();
        }
        payload.value.set(normalized);
        payload.is_dirty.set(true);
    };
    use_effect(move || {
        let Some(deep_link::DeepLinkAction::Pair {
            payload: linked_payload,
        }) = claimed_action()
        else {
            return;
        };
        claimed_action.set(None);
        apply_payload(linked_payload.as_ref());
        warn!("Applied pairing deep link payload to pairing form");
    });

    rsx! {
        div {
            class: if (payload.error)().is_some() && !payload.value.read().is_empty() { "join w-full validator-error" } else { "join w-full" },
            input {
                r#type: "text",
                id: "pairing-payload-input",
                "aria-label": "Pairing payload",
                class: "input input-sm validator join-item w-full",
                placeholder: "Paste pairing code, universal link, deep link, or shared message",
                required: true,
                value: "{payload.value}",
                oninput: move |ev| apply_payload(&ev.value()),
            }
            QrAcquireButtons {
                on_result: move |result: String| apply_payload(&result),
                on_error: move |msg: String| {
                    scan_error.set(Some(Cow::from(msg)));
                },
                scan_aria_label: "Scan pairing QR code",
                upload_aria_label: "Upload pairing QR image",
            }
        }
        if let Some(err) = scan_error() {
            div { class: "text-error", "aria-live": "polite", "{err}" }
        } else if (payload.error)().is_some() && !payload.value.read().is_empty() {
            p { class: "validator-hint text-error",
                "Invalid pairing data format. Must be a valid po:// link, po-ai.pages.dev link, or base64-encoded payload."
            }
        } else {
            p { class: "validator-hint", "Paste a link, scan a QR code, or upload a QR image" }
        }
        div {
            class: if (pin.error)().is_some() && !pin.value.read().is_empty() { "validator-error" } else { "" },
            input {
                r#type: "text",
                "aria-label": "Pairing PIN",
                class: "input input-sm validator w-full",
                placeholder: "Dynamic Pairing PIN ({PAIRING_PIN_EXACT_LEN} digits)",
                required: true,
                maxlength: PAIRING_PIN_EXACT_LEN,
                minlength: PAIRING_PIN_EXACT_LEN,
                inputmode: "numeric",
                pattern: "[0-9]*",
                value: "{pin.value}",
                oninput: move |ev| {
                    apply_form_input(pin, ev.value(), |next| {
                        reset_pair_state(scan_error, pair_error, pair_attempt);
                        if pair_gate.peek().is_some_and(|(_, cur_pin, _)| cur_pin != next) {
                            pair_gate.clear();
                        }
                    });
                },
            }
            if (pin.error)().is_some() && !pin.value.read().is_empty() {
                p { class: "validator-hint text-error", "PIN must be exactly {PAIRING_PIN_EXACT_LEN} digits" }
            } else {
                p { class: "validator-hint",
                    "Temporary pairing PIN ({PAIRING_PIN_EXACT_LEN} digits) from the other device, not your login PIN"
                }
            }
        }
        div {
            class: if (password.error)().is_some() && !password.value.read().is_empty() { "validator-error" } else { "" },
            input {
                r#type: "password",
                "aria-label": "Account password",
                class: "input input-sm validator w-full",
                placeholder: "Account password",
                autocomplete: "current-password",
                required: true,
                minlength: 8,
                maxlength: 256,
                value: "{password.value}",
                oninput: move |ev| {
                    apply_form_input(password, ev.value(), |next| {
                        reset_pair_state(scan_error, pair_error, pair_attempt);
                        if pair_gate.peek().is_some_and(|(_, _, cur_pw)| cur_pw != next) {
                            pair_gate.clear();
                        }
                    });
                },
            }
            if (password.error)().is_some() && !password.value.read().is_empty() {
                p { class: "validator-hint text-error", {(password.error)().unwrap_or_default()} }
            } else {
                p { class: "validator-hint", "Your existing account password" }
            }
        }

        label { class: "label cursor-pointer gap-4 justify-center my-4",
            input {
                r#type: "checkbox",
                class: "checkbox checkbox-xs",
                checked: remember_me,
                "aria-label": "Remember on this device",
                oninput: move |ev| {
                    reset_pair_state(scan_error, pair_error, pair_attempt);
                    pair_gate.clear();
                    remember_me.set(ev.checked());
                },
            }
            "Remember on this device"
        }

        if !cooldown_expired() {
            p {
                class: "text-center text-sm text-warning",
                role: "status",
                "Please wait {cooldown_remaining} seconds before trying again."
            }
        }
        if pair_attempt.is_busy() {
            div {
                class: "flex items-center justify-center gap-2 text-base-content/70 mt-2",
                "aria-live": "polite",
                Spinner { size: SpinnerSize::Xs }
                span {
                    {PAIRING_SYNC_STATUS.read().stage.attempt_label().unwrap_or("Pairing…")}
                }
            }
        }

        if let Some(err) = pair_error() {
            div { class: "text-center text-error", role: "alert", aria_live: "assertive", "{err}" }
            if pair_gate.peek().is_some_and(|(p, cur_pin, cur_pw)| {
                p == *payload.value.read()
                    && cur_pin == *pin.value.read()
                    && cur_pw == *password.value.read()
            }) {
                RetryButton {
                    class: "btn-xs mx-auto",
                    onclick: move |()| pair_gate.clear(),
                }
            }
        }
    }
}
