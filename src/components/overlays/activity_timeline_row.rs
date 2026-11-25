//! Activity-overlay timeline rows for pending and dead-lettered sync ops, with
//! per-row retry actions.

use std::{borrow::Cow, future::Future};

use anyhow::Result;
use dioxus::prelude::*;

use crate::{
    components::shared::format_relative_time_at,
    helpers::async_utils::now_epoch_millis,
    models::pending_operation::OpType,
    storage::background_errors::{push_background_error, report_nonfatal},
    sync::pending_ops,
};
#[component]
pub fn PendingTimelineRow(op_type: OpType, attempts: u32, scheduled_at: u64) -> Element {
    let status_text = if scheduled_at > 0 {
        format_scheduled(scheduled_at)
    } else {
        Cow::Borrowed("queued")
    };

    rsx! {
        li {
            div { class: "timeline-middle",
                span { class: "icon-[heroicons--clock] size-3 text-primary" }
            }
            div { class: "timeline-end bg-base-200 rounded-field px-3 py-2 ms-4",
                div { class: "flex items-center gap-2",
                    span { "{op_type.display_name()}" }
                    if attempts > 0 {
                        span { class: "badge badge-warning", "attempt {attempts}" }
                    }
                }
                div { class: "text-base-content/70", "{status_text}" }
            }
            hr {}
        }
    }
}

#[component]
pub fn DeadLetterTimelineRow(
    id: u64,
    op_type: OpType,
    created_at: i64,
    last_error: String,
    now_ms: u64,
) -> Element {
    let created_at_label =
        format_relative_time_at(now_ms, u64::try_from(created_at).unwrap_or_default());
    rsx! {
        li {
            div { class: "timeline-middle",
                span { class: "icon-[line-md--alert-circle] size-3 text-error" }
            }
            div { class: "timeline-end bg-error/5 border border-error/20 rounded-field px-3 py-2 ms-4",
                div { class: "flex justify-between items-start",
                    div { class: "flex-1 min-w-0",
                        div { class: "flex items-center gap-2",
                            span { "{op_type.display_name()}" }
                            span { class: "text-base-content/60", "{created_at_label}" }
                        }
                        if !last_error.is_empty() {
                            details { class: "mt-1",
                                summary { class: "text-error cursor-pointer truncate", "{last_error}" }
                                div { class: "mt-1 p-2 bg-base-200 rounded text-base-content/80 break-all text-xs select-all",
                                    "{last_error}"
                                }
                            }
                        }
                    }
                    div { class: "flex gap-1 ml-2 shrink-0",
                        button {
                            class: "btn btn-sm btn-secondary",
                            onclick: move |_| {
                                run_dlq_action(
                                    pending_ops::retry_from_dead_letter(id),
                                    "Failed to retry dead-letter operation",
                                    "Failed to retry operation. See logs for details.",
                                );
                            },
                            "Retry"
                        }
                        button {
                            class: "btn btn-sm btn-ghost",
                            onclick: move |_| {
                                run_dlq_action(
                                    pending_ops::dismiss_from_dead_letter(id),
                                    "Failed to dismiss dead-letter operation",
                                    "Failed to dismiss operation. See logs for details.",
                                );
                            },
                            "Dismiss"
                        }
                    }
                }
            }
            hr {}
        }
    }
}

/// Runs a dead-letter action in the background, reporting a failure to the log
/// and to the user-facing error banner.
fn run_dlq_action(
    task: impl Future<Output = Result<()>> + 'static,
    context: &'static str,
    user_message: &'static str,
) {
    spawn(async move {
        if let Err(e) = task.await {
            report_nonfatal("components.activity_timeline_row", context, &e);
            push_background_error("sync.dead_letter", user_message);
        }
    });
}

fn format_scheduled(scheduled_at: u64) -> Cow<'static, str> {
    let now = now_epoch_millis();
    if scheduled_at <= now {
        return Cow::Borrowed("ready");
    }
    let diff_ms = scheduled_at - now;
    let diff_secs = diff_ms / 1000;
    if diff_secs < 60 {
        Cow::Owned(format!("retry in {diff_secs}s"))
    } else {
        let diff_mins = diff_secs / 60;
        Cow::Owned(format!("retry in {diff_mins}m"))
    }
}
