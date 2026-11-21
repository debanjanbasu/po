//! `PinGate` — gpui port of `components/auth/pin_gate.rs`.
//!
//! Brute-force tracking is delegated to `user::verify_pin` which persists
//! both failed-attempt counts and cooldown timing, so remounts/restarts
//! cannot bypass lockout. Seam id: `pin-gate-input` (+ the
//! `validator-hint` feedback row). Countdown ticks via a `cx.spawn` loop
//! (the port of `use_countdown`).

use gpui::{Context, Entity, FocusHandle, Focusable, FontWeight, Render, Window, prelude::*, px};
use tracing::warn;

use crate::{
    helpers::async_utils::{now_epoch_secs, sleep_ms},
    session::{
        epoch as session,
        phase::{SESSION, abandon_second_factor, complete_admission},
    },
    ui::{
        state::session_phase,
        widgets::{
            button::{Button, ButtonVariant},
            field::{Feedback, FeedbackKind, LabeledField},
            page::CenteredPage,
        },
    },
    user::{
        self,
        password::{PinStatus, PinVerifyResult, pin_status},
        validation::validate_pin_msg,
    },
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum PinError {
    Incorrect,
    /// Internal failure — deliberately indistinguishable from other
    /// internal errors in the UI; no payload is surfaced.
    Internal,
}

impl PinError {
    const fn message(self) -> &'static str {
        match self {
            Self::Incorrect => "Incorrect PIN",
            Self::Internal => "Could not verify the PIN. Please try again.",
        }
    }
}

fn cooldown_remaining(until: u64) -> u64 {
    until.saturating_sub(now_epoch_secs())
}

fn pending_user_matches(expected: &str) -> bool {
    SESSION
        .peek()
        .pending_user()
        .is_some_and(|u| u.username() == expected)
}

pub struct PinGate {
    focus: FocusHandle,
    pin_error: Option<PinError>,
    lock_remaining: u64,
    verifying: bool,
    /// Field state created on first render (window-bound).
    pin_state: Option<Entity<gpui_ce_elements::editable_text::EditableTextState>>,
}

impl Focusable for PinGate {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl PinGate {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let this = Self {
            focus: cx.focus_handle(),
            pin_error: None,
            lock_remaining: 0,
            verifying: false,
            pin_state: None,
        };
        // Countdown ticker (the port of `use_countdown`): a detached task
        // that decrements once per second while a lock is active.
        let _this_handle = cx.entity();
        cx.spawn(async move |this, cx| {
            loop {
                sleep_ms(1_000).await;
                let stop = this
                    .update(cx, |this, cx| {
                        if this.lock_remaining > 0 {
                            this.lock_remaining -= 1;
                            cx.notify();
                            false
                        } else {
                            true
                        }
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
        })
        .detach();
        this
    }

    /// Loads the PIN attempt state for the pending user (the port of the
    /// `use_effect` on `pending_username`).
    fn load_pin_status(cx: &Context<Self>) {
        let Some(name) = session_phase(cx)
            .pending_user()
            .map(|u| u.username_arc().to_string())
        else {
            return;
        };
        let guard = session::SessionGuard::capture();
        let _this = cx.entity();
        cx.spawn(async move |this, cx| {
            let status = pin_status(&name).await;
            this.update(cx, |this, cx| {
                if !guard.is_current() || !pending_user_matches(&name) {
                    return;
                }
                match status {
                    Ok(PinStatus::Ready { .. }) => {
                        this.apply_pin_state(None, 0, false, cx);
                    }
                    Ok(PinStatus::Cooldown { until, .. }) => {
                        this.apply_pin_state(None, cooldown_remaining(until), true, cx);
                    }
                    Err(e) => {
                        warn!("Failed to load PIN attempt state for {}: {e:#}", name);
                        this.apply_pin_state(Some(PinError::Internal), 0, false, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn apply_pin_state(
        &mut self,
        error: Option<PinError>,
        remaining: u64,
        clear_pin: bool,
        cx: &mut Context<Self>,
    ) {
        self.pin_error = error;
        self.lock_remaining = remaining;
        if clear_pin && let Some(state) = &self.pin_state {
            state.update(cx, |state, cx| state.emplace("", cx));
        }
        cx.notify();
    }

    /// The verify flow (the port of the `should_verify` effect): PIN valid +
    /// lock expired + not already verifying → `user::verify_pin` with the
    /// same guard/currentness checks, then `complete_admission` on success.
    fn verify(&mut self, cx: &mut Context<Self>) {
        if self.verifying || self.lock_remaining > 0 {
            return;
        }
        // Harness seam (no-op in release builds): an unattended device
        // answers the second factor from the environment.
        if self.pin_error.is_none()
            && let Some(pin) = crate::config::e2e::account_pin()
            && let Some(state) = &self.pin_state
        {
            state.update(cx, |state, cx| state.emplace(&pin, cx));
        }

        let Some(fields) = &self.pin_state else {
            return;
        };
        let attempt = fields.read(cx).as_str().to_owned();
        if validate_pin_msg(&attempt).is_some() {
            return;
        }
        let Some(pending) = session_phase(cx).pending_user().cloned() else {
            return;
        };
        let name = pending.username().to_string();
        let guard = session::SessionGuard::capture();
        self.pin_error = None;
        self.verifying = true;
        let _this = cx.entity();
        cx.spawn(async move |this, cx| {
            let result = user::verify_pin(&name, &attempt).await;
            // Resolve the outcome on the task (admission awaited outside any
            // entity update), then apply state in one `update`.
            let current = {
                let pin_now = this
                    .update(cx, |this, cx| {
                        this.pin_state
                            .as_ref()
                            .map_or_else(String::new, |state| state.read(cx).as_str().to_owned())
                    })
                    .unwrap_or_default();
                guard.is_current() && pin_now == attempt && pending_user_matches(&name)
            };
            if !current {
                this.update(cx, |this, cx| {
                    this.verifying = false;
                    cx.notify();
                })
                .ok();
                return;
            }
            let mut apply: Option<(Option<PinError>, u64, bool)> = match &result {
                Ok(PinVerifyResult::Verified) => {
                    this.update(cx, |this, cx| {
                        this.verifying = false;
                        cx.notify();
                    })
                    .ok();
                    if let Err(error) = complete_admission(pending).await {
                        warn!("Failed to finalize admission after PIN verification: {error}");
                        Some((Some(PinError::Internal), 0, true))
                    } else {
                        None
                    }
                }
                Ok(PinVerifyResult::Incorrect { .. }) => Some((Some(PinError::Incorrect), 0, true)),
                Ok(PinVerifyResult::Cooldown { until, .. }) => {
                    Some((None, cooldown_remaining(*until), true))
                }
                Err(e) => {
                    warn!("Failed to verify PIN for {name}: {e:#}");
                    Some((Some(PinError::Internal), 0, true))
                }
            };
            if let Some((error, remaining, clear_pin)) = apply.take() {
                this.update(cx, |this, cx| {
                    this.apply_pin_state(error, remaining, clear_pin, cx);
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }
}

impl Render for PinGate {
    #[allow(clippy::too_many_lines)] // the gate is one visual unit
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Field state created on first render (window-bound).
        if self.pin_state.is_none() {
            let (pin_state, _) =
                LabeledField::create_state(&"pin-gate-input".into(), true, Some(8), window, cx);
            self.pin_state = Some(pin_state);
            // Load the attempt state once the field exists.
            Self::load_pin_status(cx);
        }

        let theme = crate::ui::theme::Theme::for_appearance(cx);
        let lock_expired = self.lock_remaining == 0;
        let username = session_phase(cx)
            .pending_user()
            .map_or_else(String::new, |u| u.username().to_string());
        let pin_feedback = self.pin_error.map(|e| Feedback {
            kind: FeedbackKind::Error,
            message: e.message().into(),
        });
        let lock_note = (self.lock_remaining > 0)
            .then(|| format!("Locked — try again in {}s", self.lock_remaining));
        let can_verify = lock_expired && !self.verifying;
        let handle = cx.entity();

        let default_hint = validate_pin_msg("").unwrap_or_default();

        CenteredPage::new().contained(true).child(
            gpui::div()
                .flex()
                .flex_col()
                .gap_3()
                .w_full()
                .max_w(px(320.0))
                .child(
                    gpui::div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.base_content)
                        .child(format!("Welcome back, {username}")),
                )
                .child(gpui::div().text_sm().text_color(theme.text_dim()).child(
                    if default_hint.is_empty() {
                        "Enter your PIN to continue".to_string()
                    } else {
                        default_hint.to_string()
                    },
                ))
                .child({
                    #[allow(clippy::expect_used)] // created above in this render
                    let state = self.pin_state.as_ref().expect("created above");
                    let mut field =
                        LabeledField::new("pin-gate-input", "PIN", "6 or more digits", state);
                    if let Some(fb) = pin_feedback {
                        field = field.feedback(Some(fb));
                    }
                    field.disabled(!lock_expired)
                })
                .children(lock_note.map(|note| {
                    gpui::div()
                        .text_xs()
                        .text_color(theme.warn_text())
                        .child(note)
                }))
                .child(
                    Button::new("pin-verify-button", "Unlock", ButtonVariant::Primary, {
                        move |_, _, cx| {
                            handle.update(cx, Self::verify);
                        }
                    })
                    .disabled(!can_verify)
                    .full_width(true),
                )
                .child(
                    Button::new(
                        "pin-different-account",
                        "Use a different account",
                        ButtonVariant::Ghost,
                        {
                            |_, _, _cx| {
                                abandon_second_factor();
                            }
                        },
                    )
                    .full_width(true),
                ),
        )
    }
}
