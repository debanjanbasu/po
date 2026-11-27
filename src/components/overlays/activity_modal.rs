//! Activity modal: overview, queue, dead-letter queue, and background tasks.

use crate::{
    components::shared::{FeedbackKind, FormFeedback, ModalDialog, use_modal_dialog_id},
    global_state::network_status,
    helpers::{async_utils::now_epoch_millis, capnp::capnp_u32},
    models::pending_operation::PendingOperation,
    nonfatal,
    storage::{
        background_errors::BACKGROUND_ERRORS,
        hydration_status::HYDRATION_STATUS,
        pairing_sync_status::PAIRING_SYNC_STATUS,
        sync_status::{PENDING_OPS_REVISION, SYNC_STATUS},
        training_status::TRAINING_STATUS,
        upgrade_status::UPGRADE_STATUS,
    },
    sync::pending_ops,
};
use dioxus::prelude::*;
use tokio::join;

use super::{
    SHOW_ACTIVITY_MODAL,
    activity_bar::{classify_network, pairing_banner_text, share_sync_banner},
    activity_panels::{ActivityStats, BackgroundErrorsPanel, BackgroundTasks},
    activity_timeline_row::{DeadLetterTimelineRow, PendingTimelineRow},
};

#[component]
pub fn ActivityModal() -> Element {
    let dialog_id = use_modal_dialog_id("activity-modal");
    let modal_open = use_memo(|| *SHOW_ACTIVITY_MODAL.read());
    let queue_data = use_resource(move || {
        let open = modal_open();
        if open {
            // Both subscriptions: the phase for open/close transitions, the
            // revision for same-count mutations (feed squash, retry) that
            // leave `SYNC_STATUS` untouched but change row contents.
            let _phase = SYNC_STATUS();
            let _revision = PENDING_OPS_REVISION();
        }
        async move {
            if !open {
                return None;
            }
            let (pending, dead_letter) =
                join!(pending_ops::list_pending(), pending_ops::list_dead_letter(),);
            Some(match (pending, dead_letter) {
                (Ok(pending_items), Ok(dead_letter_items)) => {
                    Ok((pending_items, dead_letter_items))
                }
                (pending_res, dead_letter_res) => {
                    let pending_msg = pending_res
                        .err()
                        .map_or_else(String::new, |e| format!("pending: {e}"));
                    let dead_msg = dead_letter_res
                        .err()
                        .map_or_else(String::new, |e| format!("dead-letter: {e}"));
                    let separator = if !pending_msg.is_empty() && !dead_msg.is_empty() {
                        "; "
                    } else {
                        ""
                    };
                    Err(format!(
                        "Failed to load queue data ({pending_msg}{separator}{dead_msg})"
                    ))
                }
            })
        }
    });

    if !modal_open() {
        return rsx! {};
    }

    let data = queue_data.read();
    let queue_state = data.as_ref().and_then(Option::as_ref);
    let loaded = queue_state.is_some();
    let (pending_items, dl_items): (&[PendingOperation], &[PendingOperation]) = match queue_state {
        Some(Ok((pending, dead_letter))) => (pending.as_slice(), dead_letter.as_slice()),
        None | Some(Err(_)) => (&[], &[]),
    };
    let sync_processing = SYNC_STATUS.read().is_processing();
    let (is_upgrading, upgrade_progress) = {
        let upgrade = UPGRADE_STATUS.read();
        (upgrade.is_upgrading(), upgrade.progress())
    };
    let (is_training, last_trained) = {
        let training = TRAINING_STATUS.read();
        (training.is_training(), training.last_trained())
    };
    let hydration_state = *HYDRATION_STATUS.read();
    let is_hydrating = hydration_state.is_active();
    let hydration_progress = hydration_state
        .progress()
        .map(|p| (p.completed, p.failed, p.total));

    // While a pairing sync is running, its row subsumes the hydration row
    // (conversation hydration is one of the pairing stages). The share side
    // of a pairing (another device pulling from this one) reuses the row.
    let pairing_label = pairing_banner_text(
        PAIRING_SYNC_STATUS.read().stage,
        hydration_progress.map(|(completed, _, total)| (completed, total)),
    )
    .or_else(|| share_sync_banner().map(|(text, _)| text));

    let pending_count = capnp_u32(pending_items.len());
    let dead_letter_count = capnp_u32(dl_items.len());
    let (has_connectivity, network_detail, connection_text, network_icon) = {
        let net = network_status().read();
        let (_, connection_text, network_icon) = classify_network(&net);
        (
            net.has_connectivity(),
            net.attached_detail(),
            connection_text,
            network_icon,
        )
    };
    let has_background_activity =
        is_upgrading || is_training || is_hydrating || pairing_label.is_some();
    let has_items = !pending_items.is_empty() || !dl_items.is_empty();
    let has_background_errors = !BACKGROUND_ERRORS.read().is_empty();

    rsx! {
        ModalDialog {
            dialog_id,
            title: "Activity",
            panel_class: "max-w-lg",
            show_close_button: true,
            close_button_autofocus: true,
            close_button_label: "Close activity status",
            on_close: move |()| *SHOW_ACTIVITY_MODAL.write() = false,

            ActivityStats {
                network_icon,
                connection_text,
                pending_count,
                processing: sync_processing && pending_count > 0,
                has_connectivity,
                dead_letter_count,
                network_detail,
            }

            if queue_state.is_none() {
                div { class: "mb-4 text-sm text-base-content/60", "Loading queue activity…" }
            }

            if let Some(Err(err)) = queue_state {
                FormFeedback {
                    kind: FeedbackKind::Error,
                    message: err.clone(),
                    class: "mb-4",
                    icon_class: "icon-[line-md--alert-circle-twotone-loop]",
                }
            }
            if has_items {
                div { class: "mb-4",
                    h3 { class: "mb-3 text-base-content/70", "Timeline" }
                    if !pending_items.is_empty() {
                        h4 { class: "uppercase text-base-content/60 mb-2", "Queued" }
                        ul { class: "timeline timeline-vertical timeline-compact timeline-snap-icon mb-3",
                            for op in pending_items {
                                PendingTimelineRow {
                                    key: "{op.id}",
                                    op_type: op.op_type,
                                    attempts: op.attempts,
                                    scheduled_at: op.scheduled_at,
                                }
                            }
                        }
                    }
                    if !dl_items.is_empty() {
                        div { class: "flex items-center justify-between mb-2",
                            h4 { class: "uppercase text-error", "Failed (DLQ)" }
                            div { class: "flex gap-1",
                                button {
                                    class: "btn btn-xs btn-secondary",
                                    onclick: move |_| {
                                        spawn(async move {
                                            nonfatal!(
                                                "components.activity_modal",
                                                "Failed to retry all dead-letter ops",
                                                pending_ops::retry_all_dead_letters().await
                                            );
                                        });
                                    },
                                    "Retry all"
                                }
                                button {
                                    class: "btn btn-xs btn-ghost",
                                    onclick: move |_| {
                                        spawn(async move {
                                            nonfatal!(
                                                "components.activity_modal",
                                                "Failed to dismiss all dead-letter ops",
                                                pending_ops::dismiss_all_dead_letters().await
                                            );
                                        });
                                    },
                                    "Dismiss all"
                                }
                            }
                        }
                        ul { class: "timeline timeline-vertical timeline-compact timeline-snap-icon",
                            {
                                let now_ms = now_epoch_millis();
                                rsx! {
                                    for op in dl_items {
                                        DeadLetterTimelineRow {
                                            key: "{op.id}",
                                            id: op.id,
                                            op_type: op.op_type,
                                            created_at: op.created_at,
                                            last_error: op.last_error.clone(),
                                            now_ms,
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if has_background_activity || last_trained.is_some_and(|_| !is_training) {
                BackgroundTasks {
                    training_active: is_training,
                    upgrading: is_upgrading,
                    upgrade_progress,
                    last_trained: last_trained.filter(|_| !is_training),
                    hydrating: is_hydrating && pairing_label.is_none(),
                    hydration_progress,
                    pairing_label,
                }
            }

            if has_background_errors {
                BackgroundErrorsPanel {}
            }

            if loaded && !matches!(queue_state, Some(Err(_))) && !has_items && !has_background_activity && !has_background_errors {
                div { class: "text-center py-6 text-base-content/60",
                    span { class: "icon-[line-md--confirm-circle-twotone] size-5 text-secondary" }
                    p { class: "mt-2", "All clear — nothing in the queue" }
                }
            }
        }
    }
}
