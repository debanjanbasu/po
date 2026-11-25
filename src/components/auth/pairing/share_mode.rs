//! `ShareModeSection` — existing-device side of pairing: build the QR/PIN share
//! payload and track beacon milestones until the peer completes admission.

use dioxus::prelude::*;

use crate::{
    components::{
        auth::{LatestAttemptController, use_countdown, use_latest_attempt_controller},
        shared::{QrShareCard, Spinner, SpinnerSize, use_form_field_manual},
    },
    config,
    helpers::async_utils::sleep_ms,
    platform::{share, share_urls},
    session::{epoch::SessionGuard, phase::SESSION},
    storage::background_errors::report_nonfatal,
    user::{
        pairing_beacon::{self, BeaconMilestone},
        validation::validate_password_msg,
    },
};

use super::actions::{PairingShare, ShareGenerationResult, generate_share, verify_publish_warning};
/// Latest beacon milestone from the device pairing against this QR,
/// with the label that device reported for itself.
type RemotePairStatus = Option<(BeaconMilestone, String)>;

/// Tears the share UI down: QR gone, attempt cancelled, beacon globals
/// cleared (which also clears the activity-bar mirror).
fn reset_share(
    mut share_data: Signal<Option<PairingShare>>,
    share_attempt: LatestAttemptController,
    mut hold_armed: Signal<bool>,
) {
    share_data.set(None);
    share_attempt.cancel();
    hold_armed.set(false);
    pairing_beacon::clear_active_share();
}

#[component]
pub(super) fn ShareModeSection() -> Element {
    let mut share_data = use_signal(|| None::<PairingShare>);
    let mut share_password = use_form_field_manual("", false, validate_password_msg);
    let mut share_pw_error = use_signal(|| false);
    let mut share_gen_error = use_signal(|| None::<String>);
    let mut last_password_attempt = use_signal(|| None::<String>);
    let mut remaining_secs = use_countdown(0);
    let remaining_expired = use_memo(move || remaining_secs() == 0);
    let share_attempt = use_latest_attempt_controller();
    let mut qr_generation = use_signal(|| 0_u32);
    // Validated milestone of the device pairing against this QR, derived
    // from the beacon globals (DHT-watch push lands there — the activity
    // bar derives from the same source).
    let remote_status = use_memo(pairing_beacon::share_sync_status);
    let mut publish_warning = use_signal(|| None::<String>);
    // Post-expiry hold: armed when the TTL lapses while the other device is
    // mid-sync, so progress stays visible instead of vanishing.
    let mut hold_armed = use_signal(|| false);
    let mut hold_secs = use_countdown(0);
    let hold_expired = use_memo(move || hold_secs() == 0);

    // Teardown. At TTL expiry the QR is dead, but if the other device is
    // mid-sync the UI must not vanish — the user would close the app while
    // it's still serving that sync. Hold until Complete (linger effect
    // below) or until the grace cap runs out.
    use_effect(move || {
        let expired = remaining_expired();
        let milestone = remote_status().map(|(m, _)| m);
        let hold_done = hold_expired();
        if !expired || share_data.peek().is_none() {
            return;
        }
        match milestone {
            Some(BeaconMilestone::Accepted | BeaconMilestone::Syncing) => {
                if !*hold_armed.peek() {
                    hold_armed.set(true);
                    hold_secs.set(config::pairing::SHARE_SYNC_HOLD_GRACE_SECS);
                } else if hold_done {
                    reset_share(share_data, share_attempt, hold_armed);
                }
            }
            // Completed sync stays visible; the linger effect tears down.
            Some(BeaconMilestone::Complete) => {}
            None => reset_share(share_data, share_attempt, hold_armed),
        }
    });
    // "Synced ✓" lingers briefly, then the share UI resets — the QR is
    // spent the moment a device was admitted with it.
    use_effect(move || {
        let completed = matches!(
            remote_status().map(|(m, _)| m),
            Some(BeaconMilestone::Complete)
        );
        if !completed {
            return;
        }
        let Some((pairing_id, generated_at)) = share_data
            .peek()
            .as_ref()
            .map(|sd| (sd.pairing_id, sd.generated_at))
        else {
            return;
        };
        spawn(async move {
            sleep_ms(config::pairing::SHARE_COMPLETE_LINGER_SECS * 1_000).await;
            let still_current = share_data.peek().as_ref().is_some_and(|cur| {
                cur.pairing_id == pairing_id && cur.generated_at == generated_at
            });
            if still_current {
                reset_share(share_data, share_attempt, hold_armed);
            }
        });
    });

    // Beacon delivery is push-only: the session's whole-record identity
    // watch covers the beacon subkey, and veilid natively renews watches,
    // reconciles them across reattach, and runs a periodic fallback inspect
    // that catches missed notifications — no app-level poll loop needed.
    // Milestones land in the beacon globals via `handle_watch_change`.

    // The QR renders without waiting on the network; identity readability is
    // verified (and repaired) in the background, surfacing problems as a
    // warning under the QR. One-shot per generation; the identity guard
    // keeps a stale result from landing on a regenerated code.
    use_effect(move || {
        let _generation = qr_generation();
        let Some((pairing_id, generated_at)) = share_data
            .peek()
            .as_ref()
            .map(|sd| (sd.pairing_id, sd.generated_at))
        else {
            return;
        };
        spawn(async move {
            let warning = verify_publish_warning().await;
            let still_current = share_data.peek().as_ref().is_some_and(|cur| {
                cur.pairing_id == pairing_id && cur.generated_at == generated_at
            });
            if still_current {
                publish_warning.set(warning);
            }
        });
    });
    // Auto-generate pairing QR when a valid password is entered.
    use_effect(move || {
        if share_data.read().is_some() || !(share_password.is_valid)() || share_attempt.is_busy() {
            return;
        }
        let password = (share_password.value)();
        if last_password_attempt().as_deref() == Some(password.as_str()) {
            return;
        }
        last_password_attempt.set(Some(password.clone()));
        if share_gen_error.peek().is_some() {
            share_gen_error.set(None);
        }
        if *share_pw_error.peek() {
            share_pw_error.set(false);
        }
        let attempt_id = share_attempt.begin();
        share_attempt.replace_task(spawn(async move {
            let is_current = || {
                share_attempt.matches(attempt_id)
                    && share_password.value.peek().as_str() == password
                    && SESSION.peek().authenticated_user().is_some()
            };
            let result = generate_share(&password).await;
            if !is_current() {
                return;
            }
            match result {
                Ok(ShareGenerationResult::WrongPassword) => share_pw_error.set(true),
                Ok(ShareGenerationResult::NotOnboarded) => {
                    share_gen_error.set(Some(
                        "Your identity hasn't been published to the network yet. Connect online to complete setup before pairing."
                            .to_string(),
                    ));
                }
                Ok(ShareGenerationResult::Ready(data)) => {
                    share_pw_error.set(false);
                    publish_warning.set(None);
                    hold_armed.set(false);
                    pairing_beacon::register_active_share(data.pairing_id, data.generated_at);
                    share_data.set(Some(data));
                    remaining_secs.set(config::e2e::pairing_ttl_secs());
                    qr_generation.with_mut(|value| *value = value.wrapping_add(1));
                }
                Err(e) => {
                    report_nonfatal("auth.pairing", "Failed to generate pairing payload", &e);
                    share_gen_error.set(Some(
                        "Unable to generate pairing data right now. Please try again.".to_string(),
                    ));
                }
            }
            share_attempt.finish(attempt_id);
        }));
    });

    let share_guard = share_data.read();
    rsx! {
        if share_guard.is_some() && remaining_expired() {
            // TTL lapsed while the other device was mid-sync (or showing
            // its completion): the QR is spent, but progress stays visible.
            {expired_hold_view(remote_status())}
        } else if let Some(sd) = share_guard.as_ref() {
            QrShareCard {
                key: "{qr_generation}",
                title: "Pairing QR",
                description: "Share this QR with your other device to log in.",
                webp: sd.qr_webp.clone(),
                copy_text: sd.deep_link.clone(),
                share_text: {
                    share_urls::ShareBundle {
                        title: Some("Pair your device with po".to_string()),
                        username: SESSION
                            .peek()
                            .authenticated_user()
                            .map(|u| u.username().to_owned()),
                        deep_link: Some(sd.deep_link.clone()),
                        remaining_secs: Some(config::e2e::pairing_ttl_secs()),
                    }
                    .to_multiline_text()
                },
                download_filename: "po-pairing.webp",
                details: {
                    let pin = sd.pin.clone();
                    rsx! {
                        div { class: "flex items-center justify-center gap-3",
                            button {
                                class: "badge badge-neutral gap-1 cursor-pointer",
                                "aria-label": "Copy PIN to clipboard",
                                onclick: move |_| {
                                    let pin = pin.clone();
                                    let guard = SessionGuard::capture();
                                    spawn(async move {
                                        let result = share::copy_to_clipboard(&pin).await;
                                        guard.commit(|| {
                                            if let Err(e) = result {
                                                report_nonfatal(
                                                    "auth.pairing",
                                                    "Failed to copy PIN to clipboard",
                                                    e,
                                                );
                                                share_gen_error.set(Some(
                                                    "Unable to copy the PIN. Please try again."
                                                        .to_string(),
                                                ));
                                            }
                                        });
                                    });
                                },
                                span { class: "icon-[heroicons--key]" }
                                "PIN: {pin}"
                            }
                            ShareCountdownBadge { remaining_secs }
                        }
                        {beacon_status_view(remote_status())}
                        if let Some(warning) = publish_warning.read().as_ref() {
                            p { class: "text-warning text-center text-xs mt-2", "{warning}" }
                        }
                    }
                },
            }
        } else {
            div {
                input {
                    r#type: "password",
                    class: "input input-sm validator w-full",
                    "aria-label": "Account password",
                    placeholder: "Account password",
                    autocomplete: "current-password",
                    required: true,
                    minlength: 8,
                    maxlength: 256,
                    value: "{share_password.value}",
                    oninput: move |ev| {
                        let next = ev.value();
                        if last_password_attempt.peek().as_deref() != Some(next.as_str()) {
                            last_password_attempt.set(None);
                        }
                        share_gen_error.set(None);
                        share_pw_error.set(false);
                        share_attempt.cancel();
                        share_password.value.set(next);
                        share_password.is_dirty.set(true);
                    },
                }
                p { class: "validator-hint",
                    if share_pw_error() {
                        span { class: "text-error", "Incorrect password" }
                    } else {
                        "Required to encrypt pairing payload"
                    }
                }
            }
            if share_attempt.is_busy() {
                div {
                    class: "flex items-center justify-center gap-2 text-base-content/70 mt-2",
                    "aria-live": "polite",
                    Spinner { size: SpinnerSize::Xs }
                    span { "Generating…" }
                }
            }
            if let Some(err) = share_gen_error() {
                p { class: "text-error text-center", "{err}" }
                if last_password_attempt().as_deref() == Some(share_password.value.read().as_str()) {
                    button {
                        class: "btn btn-ghost btn-xs mx-auto",
                        r#type: "button",
                        onclick: move |_| last_password_attempt.set(None),
                        "Retry"
                    }
                }
            }
        }
    }
}

#[component]
fn ShareCountdownBadge(remaining_secs: ReadSignal<u64>) -> Element {
    let secs = remaining_secs();
    rsx! {
        div { class: "badge badge-outline gap-1",
            span { class: "icon-[heroicons--clock]" }
            div { class: "countdown font-mono",
                span { style: "--value:{secs / 60};" }
            }
            span { ":" }
            div { class: "countdown font-mono",
                span { style: "--value:{secs % 60};" }
            }
        }
    }
}

/// Live milestone line for the device pairing against this QR, with a
/// keep-the-app-open hint while a sync is actively running.
fn beacon_status_view(status: RemotePairStatus) -> Element {
    let mid_sync = matches!(
        status.as_ref().map(|(m, _)| *m),
        Some(BeaconMilestone::Accepted | BeaconMilestone::Syncing)
    );
    rsx! {
        div {
            class: "flex items-center justify-center gap-2 text-sm mt-2 text-center",
            "aria-live": "polite",
            match status {
                None => rsx! {
                    Spinner { size: SpinnerSize::Xs, class: "opacity-60" }
                    span { class: "opacity-60", "Waiting for your other device…" }
                },
                Some((milestone, label)) => {
                    let device = if label.is_empty() {
                        "your other device".to_string()
                    } else {
                        label
                    };
                    match milestone {
                        BeaconMilestone::Accepted => rsx! {
                            Spinner { size: SpinnerSize::Xs }
                            span { "{device} connected — setting up…" }
                        },
                        BeaconMilestone::Syncing => rsx! {
                            Spinner { size: SpinnerSize::Xs }
                            span { "Syncing your data to {device}…" }
                        },
                        BeaconMilestone::Complete => rsx! {
                            span { class: "icon-[heroicons--check-circle] text-success size-4" }
                            span { class: "text-success", "Synced to {device}" }
                        },
                    }
                }
            }
        }
        if mid_sync {
            p { class: "text-xs opacity-60 text-center mt-1",
                "Keep this app open until the sync completes."
            }
        }
    }
}

/// Replaces the QR card when the TTL lapsed while the other device was
/// still syncing: the code is spent, but progress must stay visible so the
/// user keeps the app open until the sync lands.
fn expired_hold_view(status: RemotePairStatus) -> Element {
    let complete = matches!(
        status.as_ref().map(|(m, _)| *m),
        Some(BeaconMilestone::Complete)
    );
    let title = if complete {
        "Device sync complete"
    } else {
        "Device sync in progress"
    };
    rsx! {
        div { class: "card bg-base-100 shadow-md",
            div { class: "card-body items-center text-center gap-1",
                h3 { class: "card-title text-base", "{title}" }
                {beacon_status_view(status)}
                if !complete {
                    p { class: "text-xs opacity-60 mt-1",
                        "The pairing code expired, but your other device is still syncing."
                    }
                }
            }
        }
    }
}
