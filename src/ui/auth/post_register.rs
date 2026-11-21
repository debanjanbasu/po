//! `PostRegister` — gpui port of `components/overlays/post_register.rs`.
//!
//! The progressive-disclosure wizard: terms → DOB → PIN → biometrics, each
//! step opening as the previous one completes (`terms_open`/`dob_open`/
//! `pin_open`/`bio_open` derivation preserved). Seam ids:
//! `onboarding-terms-checkbox`, `onboarding-dob-input`,
//! `onboarding-dob-confirm-checkbox`, `onboarding-pin-input`,
//! `onboarding-biometrics-button`, `onboarding-complete-button`.

use gpui::{Context, Entity, FocusHandle, Focusable, Render, Window, prelude::*};

use crate::{
    session::epoch::SessionGuard,
    ui::{
        state::session_phase,
        widgets::{
            button::{Button, ButtonVariant},
            field::LabeledField,
            page::CenteredPage,
        },
    },
    user::{
        biometric,
        ctx::UserCtx,
        validation::{validate_dob, validate_pin_msg},
    },
};

/// The wizard draft (old `OnboardingDraft` struct, verbatim).
#[derive(Clone, Debug, Default)]
pub struct OnboardingDraft {
    pub dob: String,
    pub dob_confirmed: bool,
    pub pin: String,
    pub terms: bool,
    pub biometric_verified: bool,
}

/// Date helpers — the old module-private pair. Public so the settings
/// profile screen reuses one implementation.
pub fn yyyymmdd_to_input(v: i32) -> String {
    if v == 0 {
        String::new()
    } else {
        format!("{v}")
    }
}

pub fn input_to_yyyymmdd(input: &str) -> Option<i32> {
    let digits: String = input.chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 8 {
        return None;
    }
    digits.parse().ok()
}

pub struct PostRegister {
    focus: FocusHandle,
    draft: OnboardingDraft,
    /// Field states created on first render (window-bound).
    dob_state: Option<Entity<gpui_ce_elements::editable_text::EditableTextState>>,
    pin_state: Option<Entity<gpui_ce_elements::editable_text::EditableTextState>>,
    save_error: Option<String>,
    submitting: bool,
    attempted_bio_prompt: bool,
}

impl Focusable for PostRegister {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl PostRegister {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Seed the draft from the session's user (old `use_memo` + default).
        let session = session_phase(cx);
        let draft = session
            .authenticated_user()
            .map_or_else(OnboardingDraft::default, |u| OnboardingDraft {
                dob: yyyymmdd_to_input(u.dob_yyyymmdd()),
                dob_confirmed: u.dob_yyyymmdd() != 0,
                pin: String::new(),
                terms: u.terms_accepted(),
                biometric_verified: u.biometrics_enabled(),
            });
        Self {
            focus: cx.focus_handle(),
            draft,
            dob_state: None,
            pin_state: None,
            save_error: None,
            submitting: false,
            attempted_bio_prompt: false,
        }
    }

    fn has_existing_pin(cx: &Context<Self>) -> bool {
        session_phase(cx)
            .authenticated_user()
            .is_some_and(crate::user::model::User::has_pin)
    }

    fn username(cx: &Context<Self>) -> String {
        session_phase(cx)
            .authenticated_user()
            .map_or_else(String::new, |u| u.username().to_string())
    }

    fn pin_valid(&self, cx: &Context<Self>) -> bool {
        Self::has_existing_pin(cx) || validate_pin_msg(&self.draft.pin).is_none()
    }

    fn dob_error(&self) -> Option<&'static str> {
        input_to_yyyymmdd(&self.draft.dob).and_then(validate_dob)
    }

    fn can_continue(&self, cx: &Context<Self>) -> bool {
        self.draft.terms
            && self.draft.dob_confirmed
            && self.dob_error().is_none()
            && self.pin_valid(cx)
    }

    /// The completion submit (old `onboarding-complete-button` onclick).
    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.submitting || !self.can_continue(cx) {
            return;
        }
        self.submitting = true;
        let guard = SessionGuard::capture();
        // Read SESSION once before the draft to avoid nested reads (C-S6).
        let (name, existing_keypair) =
            UserCtx::try_current().map_or_default(|ctx| (ctx.username.to_string(), ctx.keypair));
        let dob_val = input_to_yyyymmdd(&self.draft.dob).unwrap_or_default();
        let pin_val = self.draft.pin.clone();
        let (terms, bio) = (self.draft.terms, self.draft.biometric_verified);
        let pin_opt: Option<String> = if pin_val.is_empty() {
            None
        } else {
            Some(pin_val)
        };

        let _this = cx.entity();
        let task = cx.spawn(async move |this, cx| {
            let result =
                crate::user::update_onboarding(&name, dob_val, pin_opt.as_deref(), terms, bio)
                    .await;
            this.update(cx, |this, cx| {
                if !guard.is_current() {
                    return;
                }
                match result {
                    Ok(updated) => {
                        // Preserve runtime secrets the fresh record lacks
                        // (keypair, DHT secret, password hash, sync pointers)
                        // — same field-by-field carry-over as the original.
                        crate::session::phase::mutate_authenticated_user(|u| {
                            let mut updated = updated;
                            if updated.runtime.keypair.is_none() {
                                updated.runtime.keypair =
                                    u.runtime.keypair.take().or(existing_keypair);
                            }
                            if updated.runtime.dht_secret.is_none() {
                                updated.runtime.dht_secret = u.runtime.dht_secret.take();
                            }
                            if updated.runtime.password_hash.is_none() {
                                updated.runtime.password_hash = u.runtime.password_hash.take();
                            }
                            if updated.runtime.sync_pointers.is_none() {
                                updated.runtime.sync_pointers = u.runtime.sync_pointers.take();
                            }
                            *u = updated;
                        });
                        this.submitting = false;
                    }
                    Err(err) => {
                        crate::storage::background_errors::report_nonfatal(
                            "onboarding.post_register",
                            "Failed to complete onboarding",
                            &err,
                        );
                        this.save_error = Some(err.to_string());
                        this.submitting = false;
                    }
                }
                cx.notify();
            })
            .ok();
        });
        task.detach();
        cx.notify();
    }

    /// Optional biometric prompt once PIN is valid (native only).
    fn maybe_prompt_biometrics(&mut self, cx: &Context<Self>) {
        let is_bio_available = biometric::can_check();
        let should_prompt = {
            let d = &self.draft;
            d.terms
                && d.dob_confirmed
                && self.pin_valid(cx)
                && is_bio_available
                && !d.biometric_verified
        };
        if cfg!(target_arch = "wasm32") || !should_prompt || self.attempted_bio_prompt {
            return;
        }
        self.attempted_bio_prompt = true;
        let _this = cx.entity();
        cx.spawn(async move |this, cx| {
            let verified = biometric::authenticate_async("Verify Identity").await;
            this.update(cx, |this, cx| {
                if verified {
                    this.draft.biometric_verified = true;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl PostRegister {
    /// Progressive-disclosure helper: a step renders only while open.
    fn step(open: bool, body: impl gpui::IntoElement) -> gpui::AnyElement {
        if open {
            body.into_any_element()
        } else {
            gpui::div().into_any_element()
        }
    }
}

impl Render for PostRegister {
    #[allow(clippy::too_many_lines)] // the four-step wizard is one visual unit
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Create the keyed field states on first render.
        if self.dob_state.is_none() {
            let (dob_state, _) = LabeledField::create_state(
                &"onboarding-dob-input".into(),
                false,
                Some(10),
                window,
                cx,
            );
            let (pin_state, _) = LabeledField::create_state(
                &"onboarding-pin-input".into(),
                true,
                Some(8),
                window,
                cx,
            );
            self.dob_state = Some(dob_state);
            self.pin_state = Some(pin_state);
        }

        // Pull the element text into the draft (the old two-way binding).
        if let Some(state) = &self.dob_state {
            let value = state.read(cx).as_str().to_owned();
            if value != self.draft.dob {
                self.draft.dob.clone_from(&value);
                // DOB confirmation gate flips on a complete, valid date.
                if let Some(d) = input_to_yyyymmdd(&value)
                    && validate_dob(d).is_none()
                    && !self.draft.dob_confirmed
                {
                    self.draft.dob_confirmed = true;
                }
                cx.notify();
            }
        }
        if let Some(state) = &self.pin_state {
            let value = state.read(cx).as_str().to_owned();
            if value != self.draft.pin {
                self.draft.pin = value;
                cx.notify();
            }
        }
        self.maybe_prompt_biometrics(cx);

        let theme = crate::ui::theme::Theme::for_appearance(cx);
        let has_existing_pin = Self::has_existing_pin(cx);
        let pin_valid = self.pin_valid(cx);
        let dob_yyyymmdd = input_to_yyyymmdd(&self.draft.dob);
        let dob_confirmation_disabled = dob_yyyymmdd.is_none() || self.dob_error().is_some();

        // Progressive disclosure (same derivation as the original).
        let terms_open = !self.draft.terms;
        let dob_open = self.draft.terms && !self.draft.dob_confirmed;
        let pin_open = self.draft.terms && self.draft.dob_confirmed && !pin_valid;
        let bio_open = self.draft.terms
            && self.draft.dob_confirmed
            && pin_valid
            && biometric::can_check()
            && !self.draft.biometric_verified;

        let this_handle = cx.entity();
        let can_submit = self.can_continue(cx) && !self.submitting;

        CenteredPage::new()
            .child(
                gpui::div()
                    .text_sm()
                    .text_color(theme.text_dim())
                    .text_center()
                    .mb_4()
                    .child(format!("Welcome, {}", Self::username(cx))),
            )
            .child(
                // The old `join join-vertical rounded-box shadow-md bg-base-200`.
                gpui::div()
                    .flex()
                    .flex_col()
                    .rounded(crate::ui::theme::shape::RADIUS_BOX)
                    .shadow_md()
                    .bg(theme.base_200)
                    .border_1()
                    .border_color(theme.input_border())
                    .w_full()
                    .child(Self::step(terms_open, {
                        let h = this_handle.clone();
                        gpui::div()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .text_sm()
                                    .text_color(theme.base_content)
                                    .child("Accept the terms to continue"),
                            )
                            .child(
                                gpui::div()
                                    .id("onboarding-terms-checkbox")
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(theme.base_content)
                                    .on_click(move |_, _, cx| {
                                        h.update(cx, |this, cx| {
                                            this.draft.terms = true;
                                            cx.notify();
                                        });
                                    })
                                    .child(if self.draft.terms {
                                        "[x] I accept the terms"
                                    } else {
                                        "[ ] I accept the terms"
                                    }),
                            )
                    }))
                    .child(Self::step(dob_open, {
                        #[allow(clippy::expect_used)] // created earlier in this render
                        let fields_dob = self.dob_state.clone().expect("fields created");
                        gpui::div()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(LabeledField::new(
                                "onboarding-dob-input",
                                "Date of birth (YYYYMMDD)",
                                "19900101",
                                &fields_dob,
                            ))
                            .child({
                                let h = this_handle.clone();
                                gpui::div()
                                    .id("onboarding-dob-confirm-checkbox")
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(theme.base_content)
                                    .when(dob_confirmation_disabled, |d| d.opacity(0.5))
                                    .on_click(move |_, _, cx| {
                                        h.update(cx, |this, cx| {
                                            if input_to_yyyymmdd(&this.draft.dob)
                                                .and_then(validate_dob)
                                                .is_none()
                                            {
                                                this.draft.dob_confirmed = true;
                                                cx.notify();
                                            }
                                        });
                                    })
                                    .child(if self.draft.dob_confirmed {
                                        "[x] My date of birth is correct"
                                    } else {
                                        "[ ] My date of birth is correct"
                                    })
                            })
                    }))
                    .child(Self::step(pin_open, {
                        #[allow(clippy::expect_used)] // created earlier in this render
                        let fields_pin = self.pin_state.clone().expect("fields created");
                        let pin_feedback = validate_pin_msg(&self.draft.pin).map(|m| {
                            crate::ui::widgets::field::Feedback {
                                kind: crate::ui::widgets::field::FeedbackKind::Error,
                                message: m.into(),
                            }
                        });
                        gpui::div().p_3().flex().flex_col().gap_2().child({
                            let mut field = LabeledField::new(
                                "onboarding-pin-input",
                                if has_existing_pin {
                                    "Change PIN (optional)"
                                } else {
                                    "Choose a PIN"
                                },
                                "6 or more digits",
                                &fields_pin,
                            );
                            if let Some(fb) = pin_feedback {
                                field = field.feedback(Some(fb));
                            }
                            field
                        })
                    }))
                    .when(bio_open, |col| {
                        let h = this_handle.clone();
                        col.child(
                            gpui::div()
                                .p_3()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    gpui::div()
                                        .text_sm()
                                        .text_color(theme.base_content)
                                        .child("Enable biometric unlock"),
                                )
                                .child(
                                    gpui::div()
                                        .id("onboarding-biometrics-button")
                                        .cursor_pointer()
                                        .text_sm()
                                        .text_color(theme.primary.fill)
                                        .on_click(move |_, _, cx| {
                                            h.update(cx, |this, cx| {
                                                this.attempted_bio_prompt = false;
                                                this.maybe_prompt_biometrics(cx);
                                            });
                                        })
                                        .child("Verify now"),
                                ),
                        )
                    }),
            )
            .child(
                Button::new(
                    "onboarding-complete-button",
                    "Complete setup",
                    ButtonVariant::Primary,
                    {
                        move |_, _, cx| {
                            this_handle.update(cx, Self::submit);
                        }
                    },
                )
                .disabled(!can_submit)
                .full_width(true),
            )
            .children(self.save_error.as_ref().map(|e| {
                gpui::div()
                    .text_xs()
                    .text_color(theme.error_text())
                    .child(e.clone())
            }))
    }
}
