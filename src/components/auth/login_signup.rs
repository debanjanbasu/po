//! Login and signup page. Uses a reactive auto-login pattern: credential
//! validation triggers automatically whenever the username/password signals change.

use crate::{
    LOGO,
    components::{
        auth::{
            AdmissionUi, LatestAttemptController, finalize_admission_ui,
            use_latest_attempt_controller,
        },
        shared::{CenteredPage, OrDividerLink, use_form_field_manual},
    },
    platform::deep_link,
    router::AuthRoute,
    session::phase::{SESSION, SessionPhase},
    storage::background_errors::report_nonfatal,
    user::{
        self, AuthOutcome, clear_pending_admission_effects, stage_local_auth,
        validation::{password_rule_hint, validate_password_msg, validate_username},
    },
};
use dioxus::prelude::*;
use std::borrow::Cow;

#[derive(Clone, Debug, PartialEq, Eq)]
enum AuthStatus {
    NotFound,
    Incorrect,
    Error(Cow<'static, str>),
}

type AuthAttempt = (String, String, bool);

fn reset_signup_state(
    auth_attempt: LatestAttemptController,
    signup_attempt: LatestAttemptController,
    mut signup_error: Signal<Option<String>>,
) {
    auth_attempt.cancel();
    if signup_attempt.is_busy() {
        signup_attempt.reset(clear_pending_admission_effects);
    }
    if signup_error.peek().is_some() {
        signup_error.set(None);
    }
}

/// Shared staleness check for the login effect and the signup click: the
/// attempt must still be the latest one and the captured form snapshot must
/// be unchanged.
fn snapshot_is_current(
    controller: LatestAttemptController,
    attempt_id: u64,
    attempt: &AuthAttempt,
    username_value: &Signal<String>,
    password_value: &Signal<String>,
    remember_me: Signal<bool>,
) -> bool {
    controller.matches(attempt_id)
        && attempt.0 == username_value.read().as_str()
        && attempt.1 == password_value.read().as_str()
        && attempt.2 == *remember_me.peek()
        && !matches!(*SESSION.peek(), SessionPhase::Authenticated(_))
}

#[component]
pub fn LoginSignup() -> Element {
    let nav = use_navigator();
    let mut username = use_form_field_manual("", true, |v| {
        if validate_username(v) {
            None
        } else {
            Some("3-256 characters")
        }
    });
    let mut password = use_form_field_manual("", true, validate_password_msg);
    let mut remember_me = use_signal(|| true);
    let mut status = use_signal(|| None::<AuthStatus>);
    let mut signup_error = use_signal(|| None::<String>);
    let mut last_auth_attempt = use_signal(|| None::<AuthAttempt>);

    let auth_attempt = use_latest_attempt_controller();
    let signup_attempt = use_latest_attempt_controller();

    let show_create = use_memo(move || matches!(*status.read(), Some(AuthStatus::NotFound)));

    use_effect(move || {
        // A pairing payload can arrive two ways: a deep link, or the harness
        // environment seam (a device with no typing channel). Both route to
        // the pairing form.
        if deep_link::has_pending_pairing() || crate::config::e2e::pairing_payload().is_some() {
            nav.replace(AuthRoute::Pairing);
        }
    });

    // Reactive auto-login: whenever username or password changes (and both are
    // valid), spawn a credential check. Superseded auth/signup tasks are
    // actively cancelled on input changes, any staged post-admission effects
    // are cleared before a new attempt can win, and completed results are still
    // discarded by comparing the current signals against the captured snapshot
    // before mutating auth state.
    use_effect(move || {
        if matches!(*SESSION.read(), SessionPhase::Authenticated(_)) {
            if auth_attempt.is_busy() {
                auth_attempt.cancel();
            }
            if signup_attempt.is_busy() {
                signup_attempt.cancel();
            }
            if last_auth_attempt.peek().is_some() {
                last_auth_attempt.set(None);
            }
            return;
        }
        if !(username.is_valid)() || !(password.is_valid)() || signup_attempt.is_busy() {
            if auth_attempt.is_busy() {
                auth_attempt.reset(clear_pending_admission_effects);
            }
            if status.peek().is_some() {
                status.set(None);
            }
            if last_auth_attempt.peek().is_some() {
                last_auth_attempt.set(None);
            }
            return;
        }
        let attempt = ((username.value)(), (password.value)(), remember_me());
        if last_auth_attempt.read().as_ref() == Some(&attempt) {
            return;
        }
        let attempt_id = auth_attempt.begin();
        let username_snapshot = attempt.0.clone();
        let password_snapshot = attempt.1.clone();
        let remember_snapshot = attempt.2;
        last_auth_attempt.set(Some(attempt.clone()));
        if status.peek().is_some() {
            status.set(None);
        }
        auth_attempt.replace_task(spawn(async move {
            let is_current = || {
                snapshot_is_current(
                    auth_attempt,
                    attempt_id,
                    &attempt,
                    &username.value,
                    &password.value,
                    remember_me,
                )
            };
            let result = user::check_credentials(&username_snapshot, &password_snapshot).await;
            if !is_current() {
                return;
            }
            match result {
                Ok(AuthOutcome::Authenticated(user)) => {
                    let mut user = *user;
                    stage_local_auth(&mut user, remember_snapshot);
                    match finalize_admission_ui(
                        user,
                        is_current(),
                        "auth.login_signup",
                        "Failed to finalize login",
                    )
                    .await
                    {
                        AdmissionUi::Admitted => {}
                        AdmissionUi::Superseded => return,
                        AdmissionUi::Failed => status.set(Some(AuthStatus::Error(
                            "Unable to save this login. Please try again.".into(),
                        ))),
                    }
                }
                Ok(AuthOutcome::NotFound) => {
                    status.set(Some(AuthStatus::NotFound));
                }
                Ok(AuthOutcome::Incorrect) => {
                    status.set(Some(AuthStatus::Incorrect));
                }
                Err(err) => {
                    report_nonfatal("auth.login_signup", "Credential check failed", &err);
                    status.set(Some(AuthStatus::Error(
                        "Unable to check credentials right now. Please try again.".into(),
                    )));
                }
            }
            auth_attempt.finish(attempt_id);
        }));
    });

    rsx! {
        CenteredPage {
            fieldset {
                class: "fieldset w-full max-w-xs mx-auto gap-2",
                img {
                    src: LOGO,
                    alt: "po",
                    class: "w-12 mx-auto mb-4",
                }
                div {
                    input {
                        r#type: "text",
                        class: "input input-sm validator w-full",
                        id: "login-username-input",
                        value: "{username.value}",
                        name: "username",
                        autocomplete: "username",
                        "aria-label": "Login username",
                        placeholder: "Username",
                        required: true,
                        minlength: 3,
                        maxlength: 256,
                        oninput: move |ev: FormEvent| {
                                reset_signup_state(auth_attempt, signup_attempt, signup_error);
                                username.value.set(ev.value());
                                username.is_dirty.set(true);
                            },
                    }
                    p { class: "validator-hint", "3-256 characters" }
                }
                div { class: "join w-full",
                    div { class: "grow relative",
                        input {
                            r#type: "password",
                            class: "input input-sm validator w-full",
                            id: "login-password-input",
                            value: "{password.value}",
                            name: "password",
                            autocomplete: "current-password",
                            "aria-label": "Login password",
                            placeholder: "Password",
                            required: true,
                            minlength: 8,
                            maxlength: 256,
                            pattern: r"(?=.*[0-9])(?=.*[a-z])(?=.*[A-Z])(?=.*[!@#$%\^&\*\(\)_\+\-]).{{8,256}}",
                            oninput: move |ev: FormEvent| {
                                    reset_signup_state(auth_attempt, signup_attempt, signup_error);
                                    password.value.set(ev.value());
                                    password.is_dirty.set(true);
                                },
                        }
                        p { class: "validator-hint", {password_rule_hint()} }
                    }
                    if show_create() {
                        button {
                            class: "btn btn-secondary btn-square btn-sm join-item",
                            id: "create-account-button",
                            "aria-label": "Create local user",
                            disabled: signup_attempt.is_busy(),
                            onclick: move |_| {
                                let attempt = ((username.value)(), (password.value)(), remember_me());
                                let username_snapshot = attempt.0.clone();
                                let password_snapshot = attempt.1.clone();
                                let remember_snapshot = attempt.2;
                                if auth_attempt.is_busy() {
                                    auth_attempt.cancel();
                                }
                                clear_pending_admission_effects();
                                if signup_error.peek().is_some() {
                                    signup_error.set(None);
                                }
                                let attempt_id = signup_attempt.begin();
                                signup_attempt.replace_task(spawn(async move {
                                    let is_current = || {
                                        snapshot_is_current(
                                            signup_attempt,
                                            attempt_id,
                                            &attempt,
                                            &username.value,
                                            &password.value,
                                            remember_me,
                                        )
                                    };
                                    match user::signup(&username_snapshot, &password_snapshot).await {
                                        Ok(user) => {
                                            if !is_current() {
                                                return;
                                            }
                                            let mut user = user;
                                            stage_local_auth(&mut user, remember_snapshot);
                                            match finalize_admission_ui(
                                                user,
                                                is_current(),
                                                "auth.login_signup",
                                                "Failed to finalize signup",
                                            )
                                            .await
                                            {
                                                AdmissionUi::Admitted => {}
                                                AdmissionUi::Superseded => return,
                                                AdmissionUi::Failed => signup_error.set(Some(
                                                    "Unable to save this account. Please try again."
                                                        .to_string(),
                                                )),
                                            }
                                        }
                                        Err(err) => {
                                            report_nonfatal(
                                                "auth.login_signup",
                                                "Signup failed",
                                                format!("{username_snapshot}: {err}"),
                                            );
                                            if is_current() {
                                                signup_error.set(Some(
                                                    "Unable to create account right now. Please try again."
                                                        .to_string(),
                                                ));
                                            }
                                        }
                                    }
                                    signup_attempt.finish(attempt_id);
                                }));
                            },
                            span {
                                class: "icon-[line-md--person-add-twotone]",
                                "aria-hidden": "true",
                            }
                        }
                    }
                }
                if (username.is_valid)() && (password.is_valid)() {
                    label { class: "label cursor-pointer gap-4 justify-center my-4",
                        input {
                            r#type: "checkbox",
                            class: "checkbox checkbox-xs",
                            id: "auth-remember-me-checkbox",
                            checked: remember_me,
                            "aria-label": "Remember on this device",
                            oninput: move |ev: FormEvent| {
                                reset_signup_state(auth_attempt, signup_attempt, signup_error);
                                remember_me.set(ev.checked());
                            },
                        }
                        "Remember on this device"
                    }
                }
                div { class: "text-center", "aria-live": "polite",
                    match &*status.read() {
                        None => rsx! {},
                        Some(AuthStatus::NotFound) => rsx! { "No local user found. You should create one." },
                        Some(AuthStatus::Incorrect) => rsx! { span { class: "text-error", "Credentials don't match." } },
                        Some(AuthStatus::Error(err)) => rsx! { span { class: "text-error", "{err}" } },
                    }
                    if matches!(*status.read(), Some(AuthStatus::Incorrect | AuthStatus::Error(_))) {
                        button {
                            class: "btn btn-ghost btn-sm mt-2",
                            r#type: "button",
                            onclick: move |_| last_auth_attempt.set(None),
                            "Try again"
                        }
                    }
                    if let Some(ref err) = *signup_error.read() {
                        p { class: "text-error mt-1", "{err}" }
                    }
                }
                OrDividerLink {
                    href: "/pair",
                    label: "Pair from another device",
                    icon_class: "icon-[line-md--link]",
                }
            }
        }
    }
}
