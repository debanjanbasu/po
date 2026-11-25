//! Shared state machine for the current/new/confirm credential-change forms.
//!
//! Bundles the three fields, auto-submit gating, duplicate-attempt
//! suppression, field clearing on success, and transient feedback used by
//! both the PIN and password change forms.

use dioxus::prelude::*;

use crate::{
    components::{
        settings::shell::SettingsCard,
        shared::{
            FeedbackKind, FormFeedback, FormFieldHandle, TransientSetter, use_form_field_manual,
            use_transient_message,
        },
    },
    config::ui::SUCCESS_TOAST_MS as SUCCESS_FEEDBACK_MS,
    helpers::spawn,
    session::epoch::SessionGuard,
    user::ctx::UserCtx,
};

#[derive(Clone, Copy)]
pub(super) struct ChangeForm {
    pub cur: FormFieldHandle,
    pub next: FormFieldHandle,
    pub confirm: FormFieldHandle,
    feedback: Signal<Option<(FeedbackKind, String)>>,
    /// Transient success message with its own stale-timer guard.
    success_msg: Signal<Option<String>>,
    success_set: TransientSetter,
    last_attempt: Signal<Option<(String, String)>>,
    pending: Signal<bool>,
    retryable: Signal<bool>,
    can_submit: Memo<bool>,
}

pub(super) fn use_change_form(
    cur_validator: impl Fn(&str) -> Option<&'static str> + 'static,
    next_validator: impl Fn(&str) -> Option<&'static str> + 'static,
    mismatch: &'static str,
) -> ChangeForm {
    let cur = use_form_field_manual("", true, cur_validator);
    let next = use_form_field_manual("", true, next_validator);
    let confirm = use_form_field_manual("", true, {
        let next_value = next.value;
        move |v| {
            let next = next_value.read();
            if v.is_empty() || v == next.as_str() {
                None
            } else {
                Some(mismatch)
            }
        }
    });
    let feedback = use_signal(|| None);
    let last_attempt = use_signal(|| None);
    let pending = use_signal(|| false);
    let retryable = use_signal(|| false);
    let can_submit =
        use_memo(move || (cur.is_valid)() && (next.is_valid)() && (confirm.is_valid)());
    let (success_msg, success_set) = use_transient_message(SUCCESS_FEEDBACK_MS);
    let form = ChangeForm {
        cur,
        next,
        confirm,
        feedback,
        success_msg,
        success_set,
        last_attempt,
        pending,
        retryable,
        can_submit,
    };
    super::use_account_epoch_reset(move || form.reset());
    form
}

/// Runs the shared submission checks and leaves credential-specific work to
/// the supplied async handler.
pub(super) fn use_change_submit<F, Fut>(
    username: ReadSignal<String>,
    form: &ChangeForm,
    session_error: &'static str,
    on_submit: F,
) where
    F: Fn(ChangeForm, UserCtx, String, String, String, SessionGuard) -> Fut + Copy + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let form = *form;
    use_effect(move || {
        let Some((cur, next)) = form.take_submission() else {
            return;
        };
        let name = username();
        let Some(ctx) = UserCtx::try_current().filter(|ctx| ctx.username.as_ref() == name) else {
            form.fail(session_error);
            return;
        };
        let guard = SessionGuard::capture();
        spawn(async move {
            on_submit(form, ctx, cur, next, name, guard).await;
        });
    });
}

impl ChangeForm {
    /// When all fields are valid and this `(current, new)` pair has not been
    /// attempted yet, record the attempt and return the pair.
    pub fn take_submission(mut self) -> Option<(String, String)> {
        if (self.pending)() || !(self.can_submit)() {
            return None;
        }
        let pair = ((self.cur.value)(), (self.next.value)());
        if self.last_attempt.read().as_ref() == Some(&pair) {
            return None;
        }
        self.last_attempt.set(Some(pair.clone()));
        self.feedback.set(None);
        self.retryable.set(false);
        self.pending.set(true);
        Some(pair)
    }
    pub fn succeed(mut self, message: &'static str) {
        self.reset();
        self.success_set.set(message);
    }

    fn reset(mut self) {
        for mut field in [self.cur, self.next, self.confirm] {
            field.value.set(String::new());
            field.is_dirty.set(false);
        }
        self.last_attempt.set(None);
        self.feedback.set(None);
        self.success_msg.set(None);
        self.retryable.set(false);
        self.pending.set(false);
    }

    /// Feedback currently displayed — an error wins over a transient success.
    fn shown_feedback(&self) -> Option<(FeedbackKind, String)> {
        (self.feedback)().map_or_else(
            || (self.success_msg)().map(|message| (FeedbackKind::Success, message)),
            Some,
        )
    }

    /// Show an error; the same input pair will not be resubmitted.
    pub fn fail(mut self, message: impl Into<String>) {
        self.pending.set(false);
        self.retryable.set(false);
        self.feedback
            .set(Some((FeedbackKind::Error, message.into())));
    }

    /// Offer an explicit retry; resetting the attempt here would cause an
    /// automatic retry loop in the submitting effect.
    pub fn fail_retryable(mut self, message: impl Into<String>) {
        self.fail(message);
        self.retryable.set(true);
    }
}

/// Card shell shared by the change forms: title row, fields, feedback row.
pub(super) fn change_card(title: &'static str, form: &ChangeForm, fields: Element) -> Element {
    let mut form = *form;
    rsx! {
        SettingsCard {
            li { class: "p-4 pb-0", {title} }
            li { class: "list-row",
                fieldset {
                    class: "fieldset list-col-grow gap-2",
                    disabled: (form.pending)(),
                    {fields}
                }
            }
            if (form.pending)() {
                li { class: "px-4 pb-4 pt-1 text-base-content/60", role: "status",
                    "Saving locally..."
                }
            } else if let Some((kind, message)) = form.shown_feedback() {
                li { class: "px-4 pb-4 pt-1",
                    FormFeedback { kind, message, compact: true }
                    if (form.retryable)() {
                        button {
                            class: "btn btn-ghost btn-xs mt-1",
                            r#type: "button",
                            onclick: move |_| {
                                form.last_attempt.set(None);
                                form.retryable.set(false);
                                form.feedback.set(None);
                            },
                            "Retry"
                        }
                    }
                }
            }
        }
    }
}
