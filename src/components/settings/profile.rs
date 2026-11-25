//! User profile settings page.
//! Minimal UI focusing on personal information.

use crate::{
    components::{
        settings::shell::{SettingsCard, SettingsHeader, SettingsPage},
        shared::{render_validated_date, use_form_field_manual},
    },
    helpers::spawn,
    router::Route,
    session::{epoch::SessionGuard, phase::SESSION},
    storage::background_errors::report_nonfatal,
    user::{
        self,
        validation::{input_to_yyyymmdd, validate_dob, yyyymmdd_to_input},
    },
};
use dioxus::prelude::*;

#[component]
pub fn Profile() -> Element {
    let nav = use_navigator();
    let username_memo = use_memo(|| {
        SESSION
            .read()
            .authenticated_user()
            .map(user::User::username_arc)
    });
    let current_dob = use_memo(|| {
        SESSION
            .read()
            .authenticated_user()
            .map(user::User::dob_yyyymmdd)
    });
    let (Some(username), Some(dob_yyyymmdd)) = (username_memo(), current_dob()) else {
        return rsx! { "No user found" };
    };
    let initial_input = yyyymmdd_to_input(dob_yyyymmdd);
    let mut dob = use_form_field_manual(&initial_input, true, |v: &str| {
        input_to_yyyymmdd(v).map_or(Some("Date of birth is required"), validate_dob)
    });
    let mut confirmed = use_signal(|| dob_yyyymmdd != 0);
    let mut saving = use_signal(|| false);
    let mut save_error = use_signal(|| None::<String>);
    let mut dob_conflict = use_signal(|| false);

    let mut observed_dob = use_signal(|| Some(dob_yyyymmdd));
    use_effect(move || {
        let latest_dob = current_dob();
        let latest_input = dob.value.read().clone();
        let remote_input = latest_dob.map(yyyymmdd_to_input);
        if remote_input.as_ref() == Some(&latest_input) {
            dob_conflict.set(false);
        } else if *confirmed.peek() {
            // Editing the date invalidates confirmation of a different committed value.
            confirmed.set(false);
        }
        if latest_dob != *observed_dob.peek() {
            observed_dob.set(latest_dob);
            if (dob.is_dirty)() && remote_input.as_ref() != Some(&latest_input) {
                dob_conflict.set(true);
            } else if let Some(current_dob) = latest_dob {
                let current_input = yyyymmdd_to_input(current_dob);
                dob.value.set(current_input);
                dob.is_dirty.set(false);
                confirmed.set(current_dob != 0);
                dob_conflict.set(false);
            }
        }
    });

    rsx! {
        SettingsPage {
            SettingsHeader { icon: "icon-[line-md--account]", title: "Profile Information" }

            // ── Username (Read-only) ──
            SettingsCard {
                li { class: "list-row",
                    div { class: "list-col-grow",
                        div { "Username" }
                        div { "{username}" }
                    }
                }
            }

            // ── Date of Birth ──
            SettingsCard {
                li { class: "list-row",
                    fieldset { class: "fieldset list-col-grow", disabled: saving(),
                        legend { class: "fieldset-legend", "Date of Birth" }
                        {render_validated_date(
                            &dob,
                            "Date of Birth",
                            Some("Date of birth (must be at least 13 years old)"),
                        )}
                        if dob_conflict() {
                            p { class: "text-warning text-xs", role: "status", "Your date of birth changed on another device. Your unsaved edit is preserved; review it before saving." }
                        }

                        div { class: "w-full",
                            label { class: "label cursor-pointer gap-4 mt-3 validator",
                                input {
                                    r#type: "checkbox",
                                    class: "checkbox checkbox-xs",
                                    required: true,
                                    checked: confirmed,
                                    disabled: !(dob.is_valid)() || saving(),
                                    oninput: move |ev| {
                                        let checked = ev.checked();
                                        confirmed.set(checked);
                                        if !checked || saving() {
                                            return;
                                        }
                                        let Some(dob_value) = input_to_yyyymmdd(&(dob.value)())
                                        else {
                                            confirmed.set(false);
                                            return;
                                        };

                                        save_error.set(None);
                                        saving.set(true);

                                        let uname = username.clone();
                                        let guard = SessionGuard::capture();
                                        spawn(async move {
                                            match user::update_dob(&uname, dob_value).await {
                                                Ok(()) => {
                                                    if !guard.is_current() {
                                                        return;
                                                    }
                                                    // user::update_dob already mutates SESSION via mutate_user
                                                    saving.set(false);
                                                    nav.replace(Route::Account);
                                                }
                                                Err(e) => {
                                                    if !guard.is_current() {
                                                        return;
                                                    }
                                                    confirmed.set(false);
                                                    saving.set(false);
                                                    save_error.set(Some(format!("Failed to save: {e}")));
                                                    report_nonfatal(
                                                        "settings.profile",
                                                        "Failed to persist DOB update",
                                                        format!("{uname}: {e}"),
                                                    );
                                                }
                                            }
                                        });
                                    },
                                }
                                "I confirm this is my true date of birth"
                            }
                            p { class: "validator-hint", "Please confirm" }
                            if let Some(err) = save_error.read().as_ref() {
                                p { class: "text-error mt-1", "{err}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
