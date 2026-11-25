//! Security settings page with PIN, biometric, and password management.

mod biometric_toggle;
mod change_form;

use dioxus::prelude::*;

use crate::{
    components::{
        settings::shell::{SettingsHeader, SettingsPage},
        shared::{render_validated_password, render_validated_pin},
    },
    session::{epoch::SessionGuard, phase::SESSION},
    storage::background_errors::report_nonfatal,
    user::{
        self, SecurityUpdateError, User, biometric,
        ctx::UserCtx,
        password::{PasswordChangeError, PinVerifyResult},
        validation::{password_rule_hint, validate_new_password_msg, validate_pin_msg},
    },
};

use change_form::{change_card, use_change_form, use_change_submit};
fn use_account_epoch_reset(mut reset: impl FnMut() + 'static) {
    let account = use_memo(|| {
        (
            UserCtx::try_current().map(|ctx| ctx.account_id),
            SessionGuard::capture(),
        )
    });
    use_effect(move || {
        let _ = account.read();
        reset();
    });
}

#[component]
pub fn Security() -> Element {
    // C-P2: single SESSION read via one memo; two derived memos avoid repeating the read.
    let user_data = use_memo(|| {
        SESSION
            .read()
            .authenticated_user()
            .map(|u| (u.username().to_string(), User::biometrics_enabled(u)))
    });
    let is_bio_available = biometric::can_check();
    let username = use_memo(move || user_data().map_or_else(String::new, |(u, _)| u));
    let bio_enabled = use_memo(move || user_data().is_some_and(|(_, b)| b));

    if user_data.read().is_none() {
        return rsx! {};
    }

    rsx! {
        SettingsPage {
            SettingsHeader {
                icon: "icon-[line-md--security-twotone]",
                title: "Security Settings",
            }
            PinChange { username }
            PasswordChange { username }
            if is_bio_available {
                biometric_toggle::BiometricToggle {
                    username,
                    bio_enabled,
                }
            }
        }
    }
}

#[component]
fn PinChange(username: ReadSignal<String>) -> Element {
    let form = use_change_form(validate_pin_msg, validate_pin_msg, "PINs do not match");

    use_change_submit(
        username,
        &form,
        "Sign in again before changing the PIN.",
        move |form, ctx, cur, next, name, guard| async move {
            let result = user::verify_pin_for_account(&ctx, &cur).await;
            if !guard.is_current() {
                return;
            }
            match result {
                Ok(PinVerifyResult::Verified) => {}
                Ok(PinVerifyResult::Incorrect { .. }) => {
                    return form.fail("Current PIN is incorrect");
                }
                Ok(PinVerifyResult::Cooldown { .. }) => {
                    return form.fail("Too many attempts. Wait for the PIN cooldown to expire.");
                }
                Err(e) => {
                    report_nonfatal(
                        "settings.pin_change",
                        "Failed to verify current PIN",
                        format!("{name}: {e:#}"),
                    );
                    return form.fail_retryable(
                        "Unable to verify the current PIN right now. Please try again.",
                    );
                }
            }
            let result = user::update_security(&ctx, Some(&next), None).await;
            if !guard.is_current() {
                return;
            }
            match result {
                Ok(_) => form.succeed("PIN updated locally"),
                Err(SecurityUpdateError::SyncPending(error)) => {
                    report_nonfatal(
                        "settings.pin_change",
                        "PIN updated locally; synchronization could not be queued",
                        error,
                    );
                    form.succeed("PIN updated locally. Sync needs attention in Activity.");
                }
                Err(SecurityUpdateError::Local(error)) => {
                    report_nonfatal(
                        "settings.pin_change",
                        "Failed to update PIN",
                        format!("{error:#}"),
                    );
                    form.fail_retryable("Failed to update PIN. Please try again.");
                }
            }
        },
    );

    change_card(
        "Device PIN",
        &form,
        rsx! {
            {render_validated_pin(&form.cur, "Current PIN", Some("security-pin-current"))}
            {render_validated_pin(&form.next, "New PIN", Some("security-pin-new"))}
            {render_validated_pin(&form.confirm, "Confirm PIN", Some("security-pin-confirm"))}
        },
    )
}

#[component]
fn PasswordChange(username: ReadSignal<String>) -> Element {
    let form = use_change_form(
        |_| None,
        validate_new_password_msg,
        "Passwords do not match",
    );
    use_change_submit(
        username,
        &form,
        "Sign in again before changing the password.",
        move |form, ctx, cur, next, name, guard| async move {
            let result = user::change_password(&ctx, &cur, &next).await;
            if !guard.is_current() {
                return;
            }
            match result {
                Ok(()) => form.succeed("Password updated locally"),
                Err(PasswordChangeError::IncorrectPassword) => {
                    form.fail("Incorrect current password");
                }
                Err(PasswordChangeError::InvalidNewPassword) => {
                    form.fail(format!("Use {}", password_rule_hint()));
                }
                Err(PasswordChangeError::SyncPending(error)) => {
                    report_nonfatal(
                        "settings.password_change",
                        "Password updated locally; sync queue handoff will retry",
                        error,
                    );
                    form.succeed("Password updated locally. Sync will retry automatically.");
                }
                Err(PasswordChangeError::Internal(error)) => {
                    report_nonfatal(
                        "settings.password_change",
                        "Password update failed",
                        format!("{name}: {error:#}"),
                    );
                    form.fail_retryable("Unable to update password right now. Please try again.");
                }
            }
        },
    );
    change_card(
        "Account Password",
        &form,
        rsx! {
            {render_validated_password(
                &form.cur,
                "Current Password",
                Some("current-password"),
                Some("security-password-current"),
            )}
            {render_validated_password(
                &form.next,
                "New Password",
                Some("new-password"),
                Some("security-password-new"),
            )}
            {render_validated_password(
                &form.confirm,
                "Confirm Password",
                Some("new-password"),
                Some("security-password-confirm"),
            )}
        },
    )
}
