//! Reusable panels rendered inside the activity modal: stats, background tasks,
//! empty state, and background errors.

use std::fmt::Write;

use crate::{
    components::shared::{format_compact_count, format_relative_time, format_uptime},
    storage::background_errors::{
        BACKGROUND_ERRORS, clear_background_errors, dismiss_background_error,
    },
    veilid::network::NetworkDetailLine,
};
use dioxus::prelude::*;

use super::activity_bar::NetworkIcon;

const fn network_indicator_class(network_icon: NetworkIcon) -> &'static str {
    match network_icon {
        // daisyUI `status`: 8px at the pinned --size-selector; the bare form is
        // the dim idle state.
        NetworkIcon::WifiStrong => "status status-secondary",
        NetworkIcon::SignalPartial | NetworkIcon::SignalWeak => "status status-warning",
        NetworkIcon::SignalChecking => "status",
        NetworkIcon::SignalOffline => "status status-error",
    }
}

const fn network_value_class(network_icon: NetworkIcon) -> &'static str {
    match network_icon {
        NetworkIcon::WifiStrong => "stat-value text-sm",
        NetworkIcon::SignalPartial | NetworkIcon::SignalWeak => "stat-value text-sm text-warning",
        NetworkIcon::SignalChecking => "stat-value text-sm text-base-content/70",
        NetworkIcon::SignalOffline => "stat-value text-sm text-error",
    }
}

#[component]
pub(super) fn ActivityStats(
    network_icon: NetworkIcon,
    connection_text: &'static str,
    pending_count: u32,
    processing: bool,
    has_connectivity: bool,
    dead_letter_count: u32,
    network_detail: Option<NetworkDetailLine>,
) -> Element {
    let detail_line = network_detail.map(|detail| {
        let mut line = String::new();
        let _write_result = write!(
            line,
            "{} reliable / {} live",
            detail.reliable_peer_count, detail.live_peer_count
        );
        if let Some(size) = detail.estimated_network_size {
            let _write_result = write!(line, " · net {}", format_compact_count(size));
        }
        if let Some(latency) = detail.median_latency_ms {
            let _write_result = write!(line, " · {latency}ms");
        }
        if let Some(secs) = detail.attached_uptime_secs {
            let _write_result = write!(line, " · up {}", format_uptime(secs));
        }
        line
    });
    rsx! {
        div { class: "stats stats-vertical sm:stats-horizontal shadow mb-4 w-full bg-base-200",
            div { class: "stat px-4 py-2",
                div { class: "stat-title",
                    div { class: "flex items-center gap-1",
                        span { class: network_indicator_class(network_icon) }
                        "Network"
                    }
                }
                div { class: network_value_class(network_icon), "{connection_text}" }
                if let Some(line) = detail_line {
                    div { class: "stat-desc text-base-content/60", "{line}" }
                }
            }
            div { class: "stat px-4 py-2",
                div { class: "stat-title", "Queue" }
                div { class: "stat-value text-sm", "{pending_count}" }
                if pending_count > 0 && !has_connectivity {
                    div { class: "stat-desc text-warning", "Waiting for network…" }
                } else if pending_count > 0 && processing {
                    div { class: "stat-desc text-primary", "Syncing…" }
                } else if pending_count > 0 {
                    div { class: "stat-desc", "Queued" }
                } else {
                    div { class: "stat-desc", "Up to date" }
                }
            }
            if dead_letter_count > 0 {
                div { class: "stat px-4 py-2",
                    div { class: "stat-title", "Failed (DLQ)" }
                    div { class: "stat-value text-sm text-error", "{dead_letter_count}" }
                    div { class: "stat-desc text-error", "Needs attention" }
                }
            }
        }
    }
}

/// One background-task row: icon + content, on the shared pill background.
#[component]
fn TaskRow(icon_class: &'static str, children: Element) -> Element {
    rsx! {
        li { class: "flex items-center gap-2 bg-base-200 rounded-field px-3 py-2",
            span { class: "{icon_class} size-3" }
            {children}
        }
    }
}

#[component]
pub(super) fn BackgroundTasks(
    training_active: bool,
    upgrading: bool,
    /// `(total, completed)` — total migration items and how many have finished.
    upgrade_progress: (u64, u64),
    last_trained: Option<u64>,
    hydrating: bool,
    /// `(completed, failed, total)` — hydration progress counters.
    hydration_progress: Option<(u32, u32, u32)>,
    /// Label describing an in-flight device-pairing sync stage.
    pairing_label: Option<String>,
) -> Element {
    let (total, completed) = upgrade_progress;

    rsx! {
        div { class: "mb-4",
            h4 { class: "mb-2 text-base-content/70", "Background" }
            ul { class: "space-y-2",
                if let Some(label) = pairing_label {
                    TaskRow { icon_class: "icon-[line-md--downloading-loop] text-primary",
                        span { "Device pairing: {label}" }
                    }
                }
                if hydrating {
                    if let Some((h_completed, h_failed, h_total)) = hydration_progress {
                        TaskRow { icon_class: "icon-[line-md--downloading-loop] text-primary",
                            span { "Syncing history ({h_completed}/{h_total})…" }
                            if h_failed > 0 {
                                span { class: "badge badge-xs badge-error", "{h_failed} failed" }
                            }
                        }
                    }
                }
                if training_active {
                    TaskRow { icon_class: "icon-[line-md--cog-loop] text-primary",
                        span { "Training dictionary…" }
                    }
                }
                if upgrading {
                    TaskRow { icon_class: "icon-[heroicons--arrow-path] text-primary",
                        span { "Migrating storage ({completed}/{total})…" }
                    }
                }
                if let Some(ts) = last_trained {
                    TaskRow { icon_class: "icon-[heroicons--clock] text-base-content/70",
                        span { class: "text-base-content/60", "Last trained {format_relative_time(ts)}" }
                    }
                }
            }
        }
    }
}

#[component]
pub(super) fn BackgroundErrorsPanel() -> Element {
    let errors = BACKGROUND_ERRORS.read();
    rsx! {
        div { class: "mb-4",
            div { class: "mb-2 flex items-center justify-between",
                h4 { class: "text-base-content/70", "Background Errors" }
                button {
                    class: "btn btn-ghost btn-xs",
                    onclick: move |_| clear_background_errors(),
                    "Clear all"
                }
            }
            ul { class: "space-y-2",
                // Pair the id with the row: a `move` closure cannot capture
                // `err.id` through the `&BackgroundError` (that would borrow
                // the signal guard into a `'static` handler), so bind it.
                for (id, err) in errors.iter().map(|error| (error.id, error)) {
                    li {
                        key: "{id}",
                        class: "rounded-field border border-error/30 bg-error/10 px-3 py-2",
                        div { class: "flex items-start justify-between gap-2",
                            div {
                                p { class: "text-error", "{err.domain}" }
                                p { class: "text-base-content/80 break-all", "{err.message}" }
                            }
                            button {
                                class: "btn btn-ghost btn-xs",
                                onclick: move |_| dismiss_background_error(id),
                                "Dismiss"
                            }
                        }
                    }
                }
            }
        }
    }
}
