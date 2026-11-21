//! `LoginSignup` — gpui port of `components/auth/login_signup.rs`.
//!
//! Preserves the exact orchestration contract:
//! - reactive auto-login: valid username+password spawns a credential check;
//!   superseded attempts are cancelled and their staged effects cleared;
//! - `NotFound` flips the submit button into "create account" (signup);
//! - the deep-link / harness-pairing-payload `use_effect` → route to pairing;
//! - e2e seam ids: `login-username-input`, `login-password-input`,
//!   `create-account-button`, `auth-remember-me-checkbox`.
//!
//! The dioxus signal trio (value/dirty/error per field) becomes
//! `ui::widgets::field::FormField`; the attempt controller is a plain struct
//! on the entity; the auto-login effect becomes `cx.observe` on the two
//! field-state entities + remember-me.

use anyhow::Result;
use gpui::{Context, Entity, FocusHandle, Focusable, Render, Window, prelude::*, px};

use crate::{
    platform::deep_link,
    session::phase::SessionPhase,
    ui::auth::admission::{AdmissionUi, finalize_admission_ui},
    ui::{
        router::AuthRoute,
        state::session_phase,
        widgets::{
            button::{Button, ButtonVariant},
            field::{Feedback, FormField, LabeledField},
            page::CenteredPage,
        },
    },
    user::auth::remember::stage_local_auth,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum AuthStatus {
    NotFound,
    Incorrect,
    Error(String),
}

/// `(username, password, remember_me)` — the retry-suppression key.
type AuthAttempt = (String, String, bool);

pub struct LoginSignup {
    focus: FocusHandle,
    username: FormField,
    password: FormField,
    remember_me: bool,
    status: Option<AuthStatus>,
    signup_error: Option<String>,
    last_auth_attempt: Option<AuthAttempt>,
    auth_attempt: crate::ui::auth::attempt_controller::LatestAttemptController,
    signup_attempt: crate::ui::auth::attempt_controller::LatestAttemptController,
    /// Keyed element states, created on first render (window is required).
    fields: Option<FieldStates>,
}

/// The two field states + their password sink (none here).
pub struct FieldStates {
    username_state: Entity<gpui_ce_elements::editable_text::EditableTextState>,
    password_state: Entity<gpui_ce_elements::editable_text::EditableTextState>,
}

impl Focusable for LoginSignup {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl LoginSignup {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            username: FormField::default(),
            password: FormField::default(),
            remember_me: true,
            status: None,
            signup_error: None,
            last_auth_attempt: None,
            auth_attempt: crate::ui::auth::attempt_controller::LatestAttemptController::default(),
            signup_attempt: crate::ui::auth::attempt_controller::LatestAttemptController::default(),
            fields: None,
        }
    }

    /// Creates/refreshes the keyed field states (first render) and keeps the
    /// auto-login observation alive. Returns (username, password) text.
    fn ensure_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) -> (String, String) {
        let fields = self.fields.get_or_insert_with(|| {
            let (username_state, _) = LabeledField::create_state(
                &"login-username-input".into(),
                false,
                Some(256),
                window,
                cx,
            );
            let (password_state, _) = LabeledField::create_state(
                &"login-password-input".into(),
                true,
                Some(256),
                window,
                cx,
            );
            // The reactive auto-login effect: observe both field states and
            // re-evaluate on every edit (the port of the dioxus `use_effect`).
            let u = username_state.clone();
            let p = password_state.clone();
            let this = cx.entity();
            cx.observe(&u, move |_, _, cx| {
                this.update(cx, Self::revalidate);
            })
            .detach();
            let this2 = cx.entity();
            cx.observe(&p, move |_, _, cx| {
                this2.update(cx, Self::revalidate);
            })
            .detach();
            FieldStates {
                username_state,
                password_state,
            }
        });
        (
            fields.username_state.read(cx).as_str().to_owned(),
            fields.password_state.read(cx).as_str().to_owned(),
        )
    }

    /// The port of the auto-login `use_effect` body: cancel/supersede rules
    /// and the spawned credential check with snapshot-currentness guards.
    #[allow(clippy::too_many_lines)] // the orchestration is one flow
    fn revalidate(&mut self, cx: &mut Context<Self>) {
        let Some(fields) = &self.fields else { return };
        let username_value = fields.username_state.read(cx).as_str().to_owned();
        let password_value = fields.password_state.read(cx).as_str().to_owned();

        // A pairing payload can arrive two ways: a deep link, or the harness
        // environment seam (a device with no typing channel). Both route to
        // the pairing form.
        if deep_link::has_pending_pairing() || crate::config::e2e::pairing_payload().is_some() {
            crate::ui::router::nav_auth_route(cx, AuthRoute::Pairing);
            return;
        }

        if matches!(session_phase(cx), SessionPhase::Authenticated(_)) {
            if self.auth_attempt.is_busy() {
                self.auth_attempt.cancel();
            }
            if self.signup_attempt.is_busy() {
                self.signup_attempt.cancel();
            }
            self.last_auth_attempt = None;
            return;
        }

        self.username.update(username_value.clone(), true, |v| {
            if crate::user::validation::validate_username(v) {
                None
            } else {
                Some("3-256 characters")
            }
        });
        self.password.update(
            password_value.clone(),
            true,
            crate::user::validation::validate_password_msg,
        );

        let both_valid = self.username.error.is_none() && self.password.error.is_none();
        if !both_valid || self.signup_attempt.is_busy() {
            if self.auth_attempt.is_busy() {
                self.auth_attempt
                    .reset(crate::user::clear_pending_admission_effects);
            }
            self.status = None;
            self.last_auth_attempt = None;
            cx.notify();
            return;
        }

        let attempt: AuthAttempt = (username_value, password_value, self.remember_me);
        if self.last_auth_attempt.as_ref() == Some(&attempt) {
            return;
        }

        let attempt_id = self.auth_attempt.begin();
        let username_snapshot = attempt.0.clone();
        let password_snapshot = attempt.1.clone();
        let remember_snapshot = attempt.2;
        self.last_auth_attempt = Some(attempt.clone());
        self.status = None;

        // Snapshot-currentness, closed over by the task: the entity itself
        // is the source of truth (the port of the signal reads).
        let task = cx.spawn(async move |this, cx| {
            let mut is_current = || {
                // Read current field values through the entity.
                this.update(cx, |this, cx| {
                    let Some(fields) = &this.fields else {
                        return false;
                    };
                    let u = fields.username_state.read(cx).as_str().to_owned();
                    let p = fields.password_state.read(cx).as_str().to_owned();
                    this.auth_attempt.matches(attempt_id)
                        && attempt.0 == u
                        && attempt.1 == p
                        && attempt.2 == this.remember_me
                        && !matches!(session_phase(cx), SessionPhase::Authenticated(_))
                })
                .unwrap_or(false)
            };

            let result: Result<crate::user::auth::login_signup::AuthOutcome> =
                crate::user::auth::login_signup::check_credentials(
                    &username_snapshot,
                    &password_snapshot,
                )
                .await;

            if !is_current() {
                return;
            }
            // Await the admission pipeline outside the entity update (gpui
            // update closures are sync). Classify the result first (borrow),
            // then admit + await, then apply the resolved status in one
            // `update`.
            let classified = match &result {
                Ok(crate::user::auth::login_signup::AuthOutcome::Authenticated(_)) => {
                    "authenticated"
                }
                Ok(crate::user::auth::login_signup::AuthOutcome::NotFound) => "not-found",
                Ok(crate::user::auth::login_signup::AuthOutcome::Incorrect) => "incorrect",
                Err(err) => {
                    crate::storage::background_errors::report_nonfatal(
                        "auth.login_signup",
                        "Credential check failed",
                        err,
                    );
                    "error"
                }
            };
            let mut new_status: Option<AuthStatus> = match classified {
                "not-found" => Some(AuthStatus::NotFound),
                "incorrect" => Some(AuthStatus::Incorrect),
                "error" => Some(AuthStatus::Error(
                    "Unable to check credentials right now. Please try again.".into(),
                )),
                _ => None,
            };
            let mut had_admission = false;
            if classified == "authenticated" {
                let Ok(crate::user::auth::login_signup::AuthOutcome::Authenticated(user_box)) =
                    &result
                else {
                    unreachable!("classified as authenticated")
                };
                let mut user = user_box.as_ref().clone();
                stage_local_auth(&mut user, remember_snapshot);
                if !is_current() {
                    return;
                }
                had_admission = true;
                match finalize_admission_ui(
                    user,
                    true,
                    "auth.login_signup",
                    "Failed to finalize login",
                )
                .await
                {
                    AdmissionUi::Admitted | AdmissionUi::Superseded => {}
                    AdmissionUi::Failed => {
                        new_status = Some(AuthStatus::Error(
                            "Unable to save this login. Please try again.".into(),
                        ));
                    }
                }
            }
            this.update(cx, |this, cx| {
                if had_admission {
                    this.auth_attempt.finish(attempt_id);
                }
                this.status = new_status;
                cx.notify();
            })
            .ok();
        });
        self.auth_attempt.replace_task(task);
        cx.notify();
    }

    /// The signup submit (old `create-account-button` onClick when
    /// `show_create`).
    fn submit_signup(&mut self, cx: &mut Context<Self>) {
        let username = self.username.value.clone();
        let password = self.password.value.clone();
        if self.username.error.is_some() || self.password.error.is_some() {
            return;
        }
        let _this_handle = cx.entity();
        let attempt_id = self.signup_attempt.begin();
        let task = cx.spawn(async move |this, cx| {
            let result = crate::user::signup(&username, &password).await;
            // Await the admission pipeline outside the entity update.
            let admission = match result {
                Ok(mut user) => {
                    let remember = this.update(cx, |this, _| this.remember_me).unwrap_or(true);
                    stage_local_auth(&mut user, remember);
                    Some(
                        finalize_admission_ui(
                            user,
                            true,
                            "auth.login_signup",
                            "Failed to finalize signup",
                        )
                        .await,
                    )
                }
                Err(err) => {
                    crate::storage::background_errors::report_nonfatal(
                        "auth.login_signup",
                        "Signup failed",
                        &err,
                    );
                    None
                }
            };
            this.update(cx, |this, cx| {
                if !this.signup_attempt.matches(attempt_id) {
                    return;
                }
                match admission {
                    Some(AdmissionUi::Admitted | AdmissionUi::Superseded) => {}
                    Some(AdmissionUi::Failed) | None => {
                        let message = match admission {
                            Some(AdmissionUi::Failed) => {
                                "Unable to create this account. Please try again."
                            }
                            _ => "Signup failed. Please try again.",
                        };
                        this.status = Some(AuthStatus::Error(message.into()));
                        this.signup_error = Some(message.into());
                    }
                }
                this.signup_attempt.finish(attempt_id);
                cx.notify();
            })
            .ok();
        });
        self.signup_attempt.replace_task(task);
        cx.notify();
    }
}

impl Render for LoginSignup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (username_value, password_value) = self.ensure_fields(window, cx);
        let _ = (username_value, password_value);
        let theme = crate::ui::theme::Theme::for_appearance(cx);
        let show_create = matches!(self.status, Some(AuthStatus::NotFound));

        let username_feedback = self.username.error.map(|m| Feedback {
            kind: crate::ui::widgets::field::FeedbackKind::Error,
            message: m.into(),
        });
        let password_feedback = self.password.error.map(|m| Feedback {
            kind: crate::ui::widgets::field::FeedbackKind::Error,
            message: m.into(),
        });

        let busy = self.auth_attempt.is_busy() || self.signup_attempt.is_busy();

        let this_handle = cx.entity();
        let show_create_submit = show_create;

        CenteredPage::new()
            .child(
                gpui::svg()
                    .data(crate::ui::assets::logo())
                    .size(px(48.0))
                    .my_4()
                    .text_color(crate::ui::theme::Theme::for_appearance(cx).base_content),
            )
            .child(gpui::div().flex().flex_col().gap_2().w_full().child({
                #[allow(clippy::expect_used)] // ensure_fields ran above
                let fields = self.fields.as_ref().expect("ensure_fields ran");
                LabeledField::new(
                    "login-username-input",
                    "Username",
                    "Username",
                    &fields.username_state,
                )
                .feedback(username_feedback)
            }))
            .child({
                #[allow(clippy::expect_used)] // ensure_fields ran above
                let fields = self.fields.as_ref().expect("ensure_fields ran");
                LabeledField::new(
                    "login-password-input",
                    "Password",
                    "Password",
                    &fields.password_state,
                )
                .feedback(password_feedback)
            })
            .child(
                Button::new(
                    "create-account-button",
                    if show_create {
                        "Create account"
                    } else {
                        "Sign in"
                    },
                    ButtonVariant::Primary,
                    move |_, window, cx| {
                        this_handle.update(cx, |this, cx| {
                            let _ = window;
                            if show_create_submit {
                                this.submit_signup(cx);
                            } else {
                                // Auto-login already fired; a manual press
                                // re-runs the validation effect.
                                this.revalidate(cx);
                            }
                        });
                    },
                )
                .disabled(busy)
                .full_width(true),
            )
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.text_dim())
                    .child(if show_create {
                        "Account not found — create it with these credentials."
                    } else {
                        ""
                    }),
            )
    }
}
