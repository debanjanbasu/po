//! Post-registration onboarding wizard.
//! Consolidates form state into a single signal for simplicity and performance.

use std::rc::Rc;

use dioxus::{html::MountedData, prelude::*};

use crate::{
    components::shared::CenteredPage,
    helpers::spawn,
    session::{
        epoch::SessionGuard,
        phase::{SESSION, mutate_authenticated_user},
    },
    storage::background_errors::report_nonfatal,
    user::{
        self, biometric,
        ctx::UserCtx,
        validation::{input_to_yyyymmdd, validate_dob, validate_pin_msg, yyyymmdd_to_input},
    },
};

/// Accumulated form state for the four-step onboarding wizard.
struct OnboardingDraft {
    dob: String,
    dob_confirmed: bool,
    pin: String,
    terms: bool,
    biometric_verified: bool,
}

#[component]
pub fn PostRegister() -> Element {
    let is_bio_available = biometric::can_check();

    let user_info = use_memo(move || {
        let guard = SESSION.read();
        guard.authenticated_user().map(|u| {
            (
                u.username().to_string(),
                u.has_pin(),
                u.dob_yyyymmdd(),
                u.terms_accepted(),
                u.biometrics_enabled(),
            )
        })
    });

    let Some((username, has_existing_pin, dob_yyyymmdd_val, terms_accepted, biometrics_enabled)) =
        user_info()
    else {
        return rsx! { "No session found" };
    };

    let mut draft = use_signal(|| OnboardingDraft {
        dob: yyyymmdd_to_input(dob_yyyymmdd_val),
        dob_confirmed: dob_yyyymmdd_val != 0,
        pin: String::new(),
        terms: terms_accepted,
        biometric_verified: biometrics_enabled,
    });

    let is_pin_valid =
        use_memo(move || has_existing_pin || validate_pin_msg(&draft.read().pin).is_none());
    let dob_error = use_memo(move || {
        let dob_str = &draft.read().dob;
        input_to_yyyymmdd(dob_str).and_then(validate_dob)
    });
    // PIN is always required as fallback; biometric is optional on top.
    let can_continue = use_memo(move || {
        let d = draft.read();
        d.terms && d.dob_confirmed && dob_error().is_none() && is_pin_valid()
    });

    let mut save_error = use_signal(|| None::<String>);
    let mut submitting = use_signal(|| false);
    let mut attempted_bio_prompt = use_signal(|| false);

    let (terms, dob_confirmed, biometric_verified, dob_yyyymmdd) = {
        let d = draft.read();
        (
            d.terms,
            d.dob_confirmed,
            d.biometric_verified,
            input_to_yyyymmdd(&d.dob),
        )
    };
    let dob_confirmation_disabled = dob_yyyymmdd.is_none() || dob_error().is_some();
    let pin_valid = is_pin_valid();

    // Calculate which step should be open based on progress
    let terms_open = !terms;
    let dob_open = terms && !dob_confirmed;
    let pin_open = terms && dob_confirmed && !pin_valid;
    let bio_open = terms && dob_confirmed && pin_valid && is_bio_available && !biometric_verified;

    // When progress opens the next step, move focus into its first field so
    // the user can keep typing without clicking (accepting terms focuses the
    // DOB input; confirming DOB focuses the PIN input). Tracks the last
    // auto-focused step so re-renders while a step stays open (e.g. PIN
    // keystrokes) don't re-trigger focus.
    let dob_input_el: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    let pin_input_el: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    let mut auto_focused_step = use_signal(|| None::<u8>);
    use_effect(move || {
        let (terms, dob_confirmed) = {
            let d = draft.read();
            (d.terms, d.dob_confirmed)
        };
        let pin_ok = is_pin_valid();
        let (step, el) = if terms && !dob_confirmed {
            (1_u8, dob_input_el)
        } else if terms && dob_confirmed && !pin_ok {
            (2, pin_input_el)
        } else {
            auto_focused_step.set(None);
            return;
        };
        // Subscribe (`read`, not `peek`) so a focus pending on first render
        // still fires once the input's `onmounted` delivers the element.
        let Some(el) = el.read().clone() else {
            return;
        };
        if *auto_focused_step.peek() == Some(step) {
            return;
        }
        auto_focused_step.set(Some(step));
        spawn(async move {
            let _ = el.set_focus(true).await;
        });
    });

    use_effect(move || {
        let should_prompt = {
            let d = draft.read();
            d.terms && d.dob_confirmed && is_bio_available && !d.biometric_verified
        } && is_pin_valid();

        if cfg!(target_arch = "wasm32") || !should_prompt || attempted_bio_prompt() {
            return;
        }

        attempted_bio_prompt.set(true);
        spawn(async move {
            if biometric::authenticate_async("Verify Identity").await {
                draft.write().biometric_verified = true;
            }
        });
    });

    rsx! {
        CenteredPage {
            p { class: "text-base-content/60 text-center mb-4 text-sm",
                "Welcome, "
                strong { class: "text-base-content", "{username}" }
            }

            div {
                class: "join join-vertical rounded-box shadow-md bg-base-200 w-full",
                TermsStep { draft, open: terms_open }
                DobStep {
                    dob_error: dob_error(),
                    dob_confirmation_disabled,
                    draft,
                    open: dob_open,
                    input_el: dob_input_el,
                }
                PinStep {
                    draft,
                    open: pin_open,
                    input_el: pin_input_el,
                }
                if is_bio_available {
                    BiometricsStep {
                        draft,
                        open: bio_open,
                    }
                }
            }

            button {
                class: "btn btn-sm btn-secondary w-full mt-6",
                id: "onboarding-complete-button",
                "aria-label": "Complete setup",
                disabled: !can_continue() || submitting(),
                onclick: move |_| {
                    async move {
                        if submitting() {
                            return;
                        }
                        submitting.set(true);
                        let guard = SessionGuard::capture();
                        // C-S6: read SESSION once before draft.read() to avoid nested signal reads.
                        let (name, existing_keypair) = UserCtx::try_current()
                            .map_or_default(|ctx| (ctx.username.to_string(), ctx.keypair));
                        let (dob_val, pin_val, terms, bio) = {
                            let d = draft.read();
                            (
                                input_to_yyyymmdd(&d.dob).unwrap_or_default(),
                                d.pin.clone(),
                                d.terms,
                                d.biometric_verified,
                            )
                        };

                        let pin_opt = if pin_val.is_empty() { None } else { Some(pin_val.as_str()) };
                        match user::update_onboarding(&name, dob_val, pin_opt, terms, bio).await {
                            Ok(mut updated) => {
                                if !guard.is_current() {
                                    return;
                                }
                                mutate_authenticated_user(|u| {
                                    if updated.runtime.keypair.is_none() {
                                        updated.runtime.keypair =
                                            u.runtime.keypair.take().or(existing_keypair);
                                    }
                                    if updated.runtime.dht_secret.is_none() {
                                        updated.runtime.dht_secret = u.runtime.dht_secret.take();
                                    }
                                    if updated.runtime.password_hash.is_none() {
                                        updated.runtime.password_hash =
                                            u.runtime.password_hash.take();
                                    }
                                    if updated.runtime.sync_pointers.is_none() {
                                        updated.runtime.sync_pointers =
                                            u.runtime.sync_pointers.take();
                                    }
                                    *u = updated;
                                });
                                save_error.set(None);
                            }
                            Err(e) => {
                                if !guard.is_current() {
                                    return;
                                }
                                report_nonfatal(
                                    "components.post_register",
                                    "Failed to complete onboarding",
                                    format!("{name}: {e}"),
                                );
                                save_error.set(Some(
                                    "Unable to complete setup right now. Please try again."
                                        .to_string(),
                                ));
                            }
                        }
                        submitting.set(false);
                    }
                },
                "Complete Setup"
            }

            if let Some(err) = &*save_error.read() {
                p { class: "text-error text-center mt-2", "{err}" }
            }
        }
    }
}

#[component]
fn TermsStep(mut draft: Signal<OnboardingDraft>, open: bool) -> Element {
    rsx! {
        StepCollapse {
            title: "Terms of Service & Privacy Policy",
            open,
            div { class: "space-y-3",
                p {
                    "By proceeding with registration, you acknowledge and agree to our Terms of Service and Privacy Policy."
                }
                p {
                    "We emphasize that we do not collect any personally identifiable information (PII) from users. The platform operates through a network of volunteers, and we cannot endorse or control all user contributions or content."
                }
                p {
                    "You are solely responsible for your conduct on the network and must interact with all users respectfully. We disclaim all liability for any damages, losses, or harm arising from your use of the platform."
                }
            }
            div {
                label { class: "label cursor-pointer gap-4 mt-3 validator",
                    input {
                        r#type: "checkbox",
                        class: "checkbox checkbox-xs",
                        id: "onboarding-terms-checkbox",
                        "aria-label": "Accept terms",
                        required: true,
                        checked: draft.read().terms,
                        oninput: move |ev| {
                            draft.write().terms = ev.checked();
                        },
                    }
                    "I accept the terms and conditions"
                }
                p { class: "validator-hint", "Please accept terms" }
            }
        }
    }
}

#[component]
fn DobStep(
    dob_error: Option<&'static str>,
    dob_confirmation_disabled: bool,
    mut draft: Signal<OnboardingDraft>,
    open: bool,
    mut input_el: Signal<Option<Rc<MountedData>>>,
) -> Element {
    rsx! {
        StepCollapse {
            title: "Date of Birth",
            open,
            label { class: "input input-sm validator w-full",
                input {
                    r#type: "date",
                    id: "onboarding-dob-input",
                    "aria-label": "Date of birth",
                    required: true,
                    value: "{draft.read().dob}",
                    onmounted: move |ev| input_el.set(Some(ev.data())),
                    oninput: move |ev| {
                        let mut d = draft.write();
                        d.dob = ev.value();
                        d.dob_confirmed = false;
                    },
                }
            }
            if let Some(err) = dob_error {
                p { class: "text-error mt-1", "{err}" }
            } else {
                p { class: "validator-hint", "Date of birth (must be at least 13 years old)" }
            }
            div {
                label { class: "label cursor-pointer gap-4 mt-3 validator",
                    input {
                        r#type: "checkbox",
                        class: "checkbox checkbox-xs",
                        id: "onboarding-dob-confirm-checkbox",
                        "aria-label": "Confirm date of birth",
                        required: true,
                        checked: draft.read().dob_confirmed,
                        disabled: dob_confirmation_disabled,
                        oninput: move |ev| {
                            draft.write().dob_confirmed = ev.checked();
                        },
                    }
                    "I confirm my date of birth"
                }
                p { class: "validator-hint", "Please confirm" }
            }
        }
    }
}

#[component]
fn PinStep(
    mut draft: Signal<OnboardingDraft>,
    open: bool,
    mut input_el: Signal<Option<Rc<MountedData>>>,
) -> Element {
    let pin_input_type = if cfg!(target_os = "ios") {
        "text"
    } else {
        "password"
    };
    // `.text-security-disc` (overrides.css) masks the digits on iOS, where
    // type="text" is used to avoid the password-manager prompt.
    let pin_input_class = if cfg!(target_os = "ios") {
        "text-security-disc"
    } else {
        ""
    };

    rsx! {
        StepCollapse {
            title: "Security PIN",
            open,
            div { class: "space-y-2",
                p {
                    "Create a PIN (6+ digits) for secure access. This is required even if you use biometrics."
                }
                label { class: "input input-sm validator w-full",
                    input {
                        r#type: pin_input_type,
                        id: "onboarding-pin-input",
                        "aria-label": "Security PIN",
                        inputmode: if cfg!(target_os = "ios") { "text" } else { "numeric" },
                        placeholder: "PIN",
                        class: pin_input_class,
                        required: true,
                        pattern: "[0-9]{{6,}}",
                        value: "{draft.read().pin}",
                        onmounted: move |ev| input_el.set(Some(ev.data())),
                        oninput: move |ev| {
                            draft.write().pin = ev.value();
                        },
                    }
                }
                // Keep the guidance identical to the shared PIN validator's
                // message — single source of truth for the rule wording.
                p { class: "validator-hint", {validate_pin_msg("").unwrap_or_default()} }
            }
        }
    }
}

#[component]
fn BiometricsStep(mut draft: Signal<OnboardingDraft>, open: bool) -> Element {
    let biometric_verified = draft.read().biometric_verified;
    rsx! {
        StepCollapse {
            title: "Biometric Identity",
            open,
            div { class: "flex items-center gap-4",
                div { class: "list-col-grow",
                    p {
                        "Your device supports secure biometrics. Please verify to enable secure access."
                    }
                    if biometric_verified {
                        p { class: "text-secondary mt-1 font-medium", "Identity Verified ✓" }
                    }
                }
                button {
                    class: "btn btn-sm btn-secondary btn-circle",
                    id: "onboarding-biometrics-button",
                    "aria-label": "Verify identity",
                    disabled: biometric_verified,
                    onclick: move |_| async move {
                        if biometric::authenticate_async("Verify Identity").await {
                            draft.write().biometric_verified = true;
                        }
                    },
                    span { class: "icon-[line-md--emoji-smile-twotone]" }
                }
            }
        }
    }
}

#[component]
fn StepCollapse(title: &'static str, open: bool, children: Element) -> Element {
    rsx! {
        details {
            class: "collapse collapse-arrow join-item border-b border-base-content/5",
            "name": "onboarding",
            open,
            summary { class: "collapse-title text-sm font-medium", "{title}" }
            div { class: "collapse-content space-y-2 pb-2", {children} }
        }
    }
}
