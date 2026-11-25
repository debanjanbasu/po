//! Activity bar component.
//!
//! Displays network and background activity status as a compact indicator in the top-right corner.
//! Click to open the activity modal for detailed information.

use crate::{
    components::shared::{Spinner, SpinnerSize, ThemeDropdown},
    global_state::network_status,
    helpers::capnp::capnp_u32,
    storage::{
        background_errors::BACKGROUND_ERRORS,
        hydration_status::HYDRATION_STATUS,
        pairing_sync_status::{PAIRING_SYNC_STATUS, PairingSyncStage},
        sync_status::SYNC_STATUS,
        training_status::TRAINING_STATUS,
        upgrade_status::UPGRADE_STATUS,
    },
    user::pairing_beacon::{self, BeaconMilestone},
    veilid::network::{AttachmentQuality, NetworkStatus},
};
use dioxus::prelude::*;

use super::SHOW_ACTIVITY_MODAL;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ActivitySummary {
    pub tooltip: &'static str,
    pub connection_text: &'static str,
    pub network_icon: NetworkIcon,
    pub activity_icon: ActivityIcon,
    pub badge_count: u32,
    pub has_alerts: bool,
    pub has_errors: bool,
    /// `(completed, total)` from the hydration progress report; `None` when not hydrating.
    pub hydration_progress: Option<(u32, u32)>,
    /// Current new-device pairing sync stage (`Idle` when no pairing is running).
    pub pairing_stage: PairingSyncStage,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NetworkIcon {
    WifiStrong,
    SignalPartial,
    SignalWeak,
    SignalChecking,
    SignalOffline,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ActivityIcon {
    DeadLetter,
    Hydrating,
    Upgrading,
    Training,
    Processing,
    Pending,
    None,
}

pub(super) const fn classify_network(
    net: &NetworkStatus,
) -> (&'static str, &'static str, NetworkIcon) {
    match net.attachment_quality {
        AttachmentQuality::Offline => ("Offline", "Offline", NetworkIcon::SignalOffline),
        AttachmentQuality::Connecting | AttachmentQuality::Unknown => (
            "Connecting to network",
            "Connecting",
            NetworkIcon::SignalChecking,
        ),
        AttachmentQuality::Weak => ("Weak connection", "Weak", NetworkIcon::SignalWeak),
        _ if net.is_attached && net.public_internet_ready => (
            "Fully connected",
            "Fully connected",
            NetworkIcon::WifiStrong,
        ),
        _ if net.is_attached && net.local_network_ready => (
            "Connected (local only)",
            "Connected (local)",
            NetworkIcon::SignalPartial,
        ),
        _ if net.is_attached => (
            "Attached but not ready",
            "Not ready",
            NetworkIcon::SignalWeak,
        ),
        _ => (
            "Connecting to network",
            "Connecting",
            NetworkIcon::SignalChecking,
        ),
    }
}

pub(super) fn read_activity_summary() -> ActivitySummary {
    let (tooltip, connection_text, network_icon, has_connectivity) = {
        let network = network_status().read();
        let (tooltip, connection_text, network_icon) = classify_network(&network);
        (
            tooltip,
            connection_text,
            network_icon,
            network.has_connectivity(),
        )
    };
    let (pending_count, dead_letter_count, is_processing) = {
        let sync = SYNC_STATUS.read();
        (
            sync.pending_count(),
            sync.dead_letter_count(),
            sync.is_processing(),
        )
    };
    let hydration = *HYDRATION_STATUS.read();
    let hydration_progress = hydration.progress().map(|p| (p.completed, p.total));
    let is_hydrating = hydration.is_active();
    let pairing_stage = PAIRING_SYNC_STATUS.read().stage;
    let is_upgrading = UPGRADE_STATUS.read().is_upgrading();
    let is_training = TRAINING_STATUS.read().is_training();
    let error_count = capnp_u32(BACKGROUND_ERRORS.read().len());
    let has_errors = error_count > 0;
    let has_alerts = dead_letter_count > 0 || has_errors;
    let badge_count = pending_count
        .saturating_add(dead_letter_count)
        .saturating_add(error_count);
    let activity_icon = if has_alerts {
        ActivityIcon::DeadLetter
    } else if is_hydrating || pairing_stage.is_active() {
        ActivityIcon::Hydrating
    } else if is_upgrading {
        ActivityIcon::Upgrading
    } else if is_training {
        ActivityIcon::Training
    } else if is_processing && has_connectivity {
        // Only claim active processing when veilid has a peer to talk to;
        // held-while-offline ops show as Pending instead.
        ActivityIcon::Processing
    } else if pending_count > 0 {
        ActivityIcon::Pending
    } else {
        ActivityIcon::None
    };

    ActivitySummary {
        tooltip,
        connection_text,
        network_icon,
        activity_icon,
        badge_count,
        has_alerts,
        has_errors,
        hydration_progress,
        pairing_stage,
    }
}

/// Banner text for an in-flight pairing sync; folds live conversation
/// counts into the `Conversations` stage. `None` when nothing to show.
pub(super) fn pairing_banner_text(
    stage: PairingSyncStage,
    hydration_progress: Option<(u32, u32)>,
) -> Option<String> {
    let label = stage.banner_label()?;
    if stage == PairingSyncStage::Conversations
        && let Some((completed, total)) = hydration_progress
        && total > 0
    {
        return Some(format!(
            "Syncing conversations ({completed}/{total})\u{2026}"
        ));
    }
    Some(label.to_owned())
}

/// Banner text for a device currently syncing *from* this one (share-side
/// pairing beacon). `None` when no pairing share is active. Reads global
/// signals, so callers re-render as milestones arrive — even after the
/// pairing screen unmounts.
pub(super) fn share_sync_banner() -> Option<(String, BeaconMilestone)> {
    pairing_beacon::share_sync_status().map(|(milestone, label)| {
        let device = if label.is_empty() {
            "your other device".to_owned()
        } else {
            label
        };
        let text = match milestone {
            BeaconMilestone::Accepted => format!("Setting up {device}\u{2026}"),
            BeaconMilestone::Syncing => format!("Syncing your data to {device}\u{2026}"),
            BeaconMilestone::Complete => format!("Synced to {device}"),
        };
        (text, milestone)
    })
}

/// Pill chrome for a sync-status banner: green check once complete, an
/// in-progress spinner style otherwise.
const fn sync_pill(complete: bool) -> (&'static str, &'static str) {
    if complete {
        (
            "flex items-center gap-2 bg-success/90 text-success-content backdrop-blur-sm rounded-full px-4 py-1 shadow-md",
            "icon-[heroicons--check-circle] size-4",
        )
    } else {
        (
            "flex items-center gap-2 bg-secondary/90 text-secondary-content backdrop-blur-sm rounded-full px-4 py-1 shadow-md",
            "icon-[line-md--downloading-loop] size-4",
        )
    }
}

/// C-M4: network status icon. Checking renders the app-standard [`Spinner`];
/// the other states map to semantic icon classes.
fn network_icon(icon: NetworkIcon) -> Element {
    let class = match icon {
        NetworkIcon::WifiStrong => "icon-[heroicons--wifi] text-secondary",
        NetworkIcon::SignalPartial => "icon-[heroicons--adjustments-horizontal] text-warning",
        NetworkIcon::SignalWeak => "icon-[heroicons--signal] text-warning",
        NetworkIcon::SignalOffline => "icon-[heroicons--signal-slash] text-error",
        NetworkIcon::SignalChecking => {
            return rsx! {
                Spinner { size: SpinnerSize::Xs, class: "text-base-content/60" }
            };
        }
    };
    rsx! {
        span { class }
    }
}

/// Top space a page must leave free for the floating activity bar: the bar's own
/// top offset (`max(0.5rem, safe-area)`) plus its control row (1.5rem) plus one
/// gap. `NavLayout` puts it on the scroll container, so no page header — or any
/// other top-right control — can land underneath the bar.
///
/// The value lives in `overrides.css` (`.top-strip-padding`) because it is
/// `env()` arithmetic shared across three components — a class string would
/// repeat it in each one.
pub const TOP_STRIP_PADDING: &str = "top-strip-padding";

#[component]
pub fn ActivityBar() -> Element {
    let summary = use_memo(read_activity_summary);
    let share_banner = use_memo(share_sync_banner);
    let summary = summary();
    // Built here rather than inline in the attribute: an `if`/`else` whose branch
    // string carries a `{}` placeholder reads to clippy as a stray format
    // argument (`suspicious_else_formatting`) once the macro expands.
    let needs_attention = summary.badge_count > 0;
    let activity_label = if needs_attention {
        format!(
            "Show activity status \u{2014} {} items need attention",
            summary.badge_count
        )
    } else {
        "Show activity status".to_owned()
    };
    let attention_text = if needs_attention {
        format!("{} background items need attention", summary.badge_count)
    } else {
        String::new()
    };
    let network_icon = network_icon(summary.network_icon);
    let activity_icon = match summary.activity_icon {
        ActivityIcon::DeadLetter => Some(rsx! {
            span { class: "icon-[line-md--alert-circle] text-error" }
        }),
        ActivityIcon::Hydrating => Some(rsx! {
            span { class: "icon-[line-md--downloading-loop] text-secondary" }
        }),
        ActivityIcon::Upgrading => Some(rsx! {
            span { class: "icon-[heroicons--arrow-path] text-secondary" }
        }),
        ActivityIcon::Training => Some(rsx! {
            span { class: "icon-[line-md--cog-loop] text-secondary" }
        }),
        ActivityIcon::Processing => Some(rsx! {
            Spinner { size: SpinnerSize::Xs, class: "text-secondary" }
        }),
        ActivityIcon::Pending => Some(rsx! {
            span { class: "icon-[heroicons--cloud-arrow-up] text-secondary" }
        }),
        ActivityIcon::None => None,
    };

    let badge_class = if summary.has_alerts {
        "badge badge-xs badge-error"
    } else {
        "badge badge-xs badge-secondary"
    };
    // Pairing banner wins over the plain hydration banner: during a pairing
    // sync the conversation hydration is part of the pairing flow, and two
    // stacked banners would say the same thing twice. A failed run keeps the
    // banner visible but must not wear the green completion pill.
    let banner = pairing_banner_text(summary.pairing_stage, summary.hydration_progress)
        .map(|label| {
            let complete = summary.pairing_stage == PairingSyncStage::Complete;
            let failed = summary.pairing_stage.is_failed();
            let (pill_class, icon_class) = if failed {
                ("flex items-center gap-2 bg-error/90 text-error-content backdrop-blur-sm rounded-full px-4 py-1 shadow-md", "icon-[heroicons--exclamation-triangle] size-4")
            } else {
                sync_pill(complete)
            };
            (label, pill_class, icon_class)
        })
        .or_else(|| {
            // Share side of a device pairing: another device is pulling its
            // data from this one — visible app-wide so the user doesn't
            // close the app mid-sync after leaving the pairing screen.
            share_banner().map(|(label, milestone)| {
                let (pill_class, icon_class) = sync_pill(milestone == BeaconMilestone::Complete);
                (label, pill_class, icon_class)
            })
        })
        .or_else(|| {
            summary.hydration_progress.map(|(completed, total)| {
                let label = if total > 0 {
                    format!("Syncing history ({completed}/{total})\u{2026}")
                } else {
                    "Syncing history\u{2026}".to_owned()
                };
                let (pill_class, icon_class) = sync_pill(false);
                (label, pill_class, icon_class)
            })
        });

    // One top row, banner plus controls. On a wide screen the pill is
    // positioned independently and centred on the page's axis; on a narrow
    // screen it stays in the flex row (a centred pill clear of the controls
    // would truncate to "Sy…" on a phone). Both cases, and the safe-area and
    // rail insets, live in `overrides.css` (`.activity-bar`,
    // `.activity-bar-banner`) — project geometry, not reusable utilities.
    rsx! {
        div {
            class: "activity-bar fixed z-50 flex items-center justify-end gap-2 pointer-events-none",
            if let Some((label, pill_class, icon_class)) = banner {
                div { class: "activity-bar-banner", "aria-live": "polite",
                    div { class: "{pill_class} min-w-0 max-w-full pointer-events-auto",
                        span { class: "{icon_class} shrink-0" }
                        span { class: "truncate", "{label}" }
                    }
                }
            }
            div { class: "flex shrink-0 items-center gap-1 pointer-events-auto",
                ThemeDropdown { compact: true }
                // The button's `aria-label` replaces its contents, so the badge
                // count is invisible to assistive tech; background failures have
                // no other channel (they are only in the activity modal), so the
                // count is announced here as it changes.
                span { class: "sr-only", role: "status",
                    if needs_attention {
                        "{attention_text}"
                    }
                }
                button {
                    class: "flex items-center gap-2 bg-base-200/80 backdrop-blur-sm rounded-full px-3 py-1 shadow-sm cursor-pointer border border-base-300",
                    title: summary.tooltip,
                    "aria-label": "{activity_label}",
                    onclick: move |_| *SHOW_ACTIVITY_MODAL.write() = true,

                    {network_icon}

                    if let Some(icon) = activity_icon {
                        {icon}
                    }

                    if summary.badge_count > 0 {
                        span { class: badge_class, "{summary.badge_count}" }
                    }
                }
            }
        }
    }
}
