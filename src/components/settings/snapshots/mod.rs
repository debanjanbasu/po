//! Data & Backups — export encrypted backups, restore on another device, and
//! manage automatic snapshot retention.
//!
//! Accessible from Account → Data & Backups.

mod delete_modal;
mod export;
mod import;
mod plan;
mod restore_modal;
mod shared;

use anyhow::{Result as AnyResult, anyhow, bail};
use dioxus::prelude::{ReadableExt, *};

use crate::{
    components::{
        settings::shell::{SettingsHeader, SettingsPage},
        shared::{
            close_modal_dialog, format_binary_bytes, format_local_ymdhm_from_millis,
            format_relative_time_at, use_modal_dialog_id,
        },
    },
    helpers::{async_utils::now_epoch_millis, spawn},
    models::{app_state::AppState, pending_operation::OpType},
    session::epoch::SessionGuard,
    storage::{
        background_errors::report_nonfatal,
        schema_version::SchemaVersionError,
        snapshots::{self, MAX_SNAPSHOTS, RetentionPolicy, SnapshotEntry},
        store_trait::{load_local, use_model_revision},
    },
    sync::mutate::{MutationResult, mutate},
    user::ctx::UserCtx,
};

use delete_modal::RetentionConfirmModal;
use export::ExportSection;
use import::{ImportMergeModal, ImportPasswordModal, ImportSection};
use restore_modal::RestoreConfirmModal;
use shared::{
    BackupSections, ImportState, SyncSurface, create_surface_snapshot, list_surface_snapshots,
    restore_surface_snapshot,
};

/// Returns a numeric rank for retention strictness (lower = stricter).
const fn retention_rank(p: RetentionPolicy) -> u8 {
    match p {
        RetentionPolicy::Days30 => 0,
        RetentionPolicy::Days90 => 1,
        RetentionPolicy::Forever => 2,
    }
}

/// Persists the new policy, prunes snapshots, and refreshes the list.
fn apply_retention_policy(
    policy: RetentionPolicy,
    mut error: Signal<Option<String>>,
    mut busy: Signal<bool>,
) {
    if busy() {
        return;
    }
    let Some(ctx) = UserCtx::try_current() else {
        error.set(Some("No active session".to_string()));
        return;
    };
    let guard = SessionGuard::capture();
    busy.set(true);
    error.set(None);
    spawn(async move {
        let result: AnyResult<()> = try {
            mutate::<AppState, _>(&ctx, |state| {
                state.retention_policy = policy;
                Ok(MutationResult::modified(()))
            })
            .await?;
            snapshots::apply_retention_all(ctx.storage_key()).await?;
        };
        if let Err(detail) = result {
            report_nonfatal(
                "settings.snapshots",
                "Failed to update retention policy",
                detail,
            );
            guard.commit(|| {
                error.set(Some(
                    "Could not finish updating backup retention. See Activity for details."
                        .to_string(),
                ));
            });
        }
        // Component-local busy flag: clear it even if the session changed,
        // or the retention selector stays disabled with a stuck "Saving...".
        busy.set(false);
    });
}

/// Data & Backups page — export, restore, and snapshot management.
#[component]
pub fn Snapshots() -> Element {
    let retention_dialog_id = use_modal_dialog_id("snapshot-retention-dialog");
    let active_tab = use_signal(|| SyncSurface::Totp);
    let retention_revision = use_model_revision::<AppState>();
    let retention_error = use_signal(|| None::<String>);
    let retention_busy = use_signal(|| false);
    let pending_policy = use_signal(|| None::<RetentionPolicy>);

    // Export state
    let export_error = use_signal(|| None::<String>);
    let exporting = use_signal(|| false);

    // Import state
    let import_dialog_id = use_modal_dialog_id("backup-import-dialog");
    let import_merge_dialog_id = use_modal_dialog_id("backup-merge-dialog");
    let mut import_state = ImportState {
        file_bytes: use_signal(|| None::<Vec<u8>>),
        password: use_signal(String::new),
        error: use_signal(|| None::<String>),
        importing: use_signal(|| false),
        sections: use_signal(|| None::<BackupSections>),
        success: use_signal(|| false),
    };

    let retention = use_resource(move || {
        retention_revision();
        let ctx = UserCtx::try_current();
        async move {
            let Some(ctx) = ctx else {
                return Err("Sign in to manage backup retention.".to_string());
            };
            match load_local::<AppState>(ctx.storage_key()).await {
                Ok(state) => Ok(state.retention_policy),
                Err(error) => {
                    report_nonfatal(
                        "settings.snapshots",
                        "Failed to load retention policy",
                        error,
                    );
                    Err("Could not load backup retention settings.".to_string())
                }
            }
        }
    });

    // Close modals on successful import.
    let import_dialog_id_for_effect = import_dialog_id.clone();
    let import_merge_dialog_id_for_effect = import_merge_dialog_id.clone();
    use_effect(move || {
        if (import_state.success)() {
            close_modal_dialog(&import_dialog_id_for_effect);
            close_modal_dialog(&import_merge_dialog_id_for_effect);
            import_state.file_bytes.set(None);
            import_state.sections.set(None);
            import_state.success.set(false);
        }
    });

    rsx! {
        SettingsPage {
            SettingsHeader {
                icon: "icon-[line-md--cloud-down-twotone]",
                title: "Data & Backups",
                subtitle: "Transfer your data between devices or keep an offline copy.",
            }

            ExportSection { exporting, export_error }
            ImportSection {
                file_bytes: import_state.file_bytes,
                password: import_state.password,
                error: import_state.error,
            }

            SnapshotHistory {
                active_tab,
                retention,
                retention_error,
                retention_busy,
                pending_policy,
            }
        }

        RetentionConfirmModal {
            retention_dialog_id,
            pending_policy,
            retention_busy,
            on_confirm: move |policy| apply_retention_policy(policy, retention_error, retention_busy),
        }

        ImportPasswordModal { import_dialog_id, state: import_state }

        ImportMergeModal { import_merge_dialog_id, state: import_state }
    }
}

/// Collapsible snapshot history with tabs, retention selector, and manual snapshot.
#[component]
fn SnapshotHistory(
    mut active_tab: Signal<SyncSurface>,
    retention: Resource<Result<RetentionPolicy, String>>,
    mut retention_error: Signal<Option<String>>,
    retention_busy: Signal<bool>,
    mut pending_policy: Signal<Option<RetentionPolicy>>,
) -> Element {
    let mut create_error = use_signal(|| None::<String>);
    let mut creating = use_signal(|| false);
    let mut restoring = use_signal(|| false);
    let mut restore_error = use_signal(|| None::<String>);
    let mut restore_success = use_signal(|| None::<String>);
    // Snapshot pending confirmation: (surface, timestamp).
    let mut pending_restore: Signal<Option<(SyncSurface, u64)>> = use_signal(|| None);
    let restore_dialog_id = use_modal_dialog_id("snapshot-restore-dialog");

    let entries = use_resource(move || {
        let tab = active_tab();
        let _ = snapshots::revision();
        let ctx = UserCtx::try_current();
        async move {
            let Some(ctx) = ctx else {
                return (tab, Err("Sign in to view snapshot history.".to_string()));
            };
            let result = list_surface_snapshots(tab, ctx.storage_key()).await;
            if let Err(error) = &result {
                report_nonfatal(
                    "settings.snapshots",
                    "Failed to load snapshot history",
                    error,
                );
            }
            (
                tab,
                result.map_err(|_| {
                    "Could not load snapshot history. See Activity for details.".to_string()
                }),
            )
        }
    });

    let create_snapshot = move |_| async move {
        if creating() {
            return;
        }
        creating.set(true);
        create_error.set(None);
        let guard = SessionGuard::capture();
        let result: Result<(), String> = try {
            let ctx = UserCtx::try_current().ok_or_else(|| "No active session".to_string())?;
            let storage_key = ctx.storage_key();
            create_surface_snapshot(active_tab(), storage_key).await?;
        };

        if guard.is_current()
            && let Err(msg) = result
        {
            report_nonfatal(
                "settings.snapshots",
                "Manual snapshot creation failed",
                &msg,
            );
            create_error.set(Some(msg));
        }
        // Component-local busy flag: clear it even if the session changed.
        creating.set(false);
    };

    let restore_snapshot = move |surface: SyncSurface, timestamp: u64, dialog_id: String| async move {
        if restoring() {
            return;
        }
        restoring.set(true);
        restore_error.set(None);
        restore_success.set(None);
        let guard = SessionGuard::capture();
        let result: AnyResult<bool> = (async {
            let ctx = UserCtx::try_current().ok_or_else(|| anyhow!("No active session"))?;
            let storage_key = ctx.storage_key();
            let Some(_user_lock) = ctx.lock_for_account_commit(guard).await? else {
                bail!("Session changed during restore");
            };
            let restored = restore_surface_snapshot(surface, storage_key, timestamp).await?;
            if restored {
                // Re-publish the restored state so other devices converge.
                ctx.enqueue_touch(match surface {
                    SyncSurface::Totp => OpType::Totp,
                    SyncSurface::Contacts => OpType::ChatContacts,
                    SyncSurface::Conversations => OpType::Chat,
                })
                .await?;
            }
            Ok(restored)
        })
        .await;
        if guard.is_current() {
            match result {
                Ok(true) => {
                    restore_success.set(Some(format!(
                        "Restored to {}.",
                        format_local_ymdhm_from_millis(timestamp)
                    )));
                }
                Ok(false) => {
                    restore_success.set(Some("Already at this snapshot.".to_string()));
                }
                Err(error) => {
                    report_nonfatal("settings.snapshots", "Snapshot restore failed", &error);
                    let message = if error.downcast_ref::<SchemaVersionError>().is_some() {
                        "This snapshot was written by a different app version and can't be restored."
                    } else {
                        "Restore failed. See Activity for details."
                    };
                    restore_error.set(Some(message.to_string()));
                }
            }
        }
        // Component-local state: clear it on every path, or a session
        // change mid-restore leaves the modal open and every Restore
        // button disabled. Only the copy above is session-gated.
        close_modal_dialog(&dialog_id);
        pending_restore.set(None);
        restoring.set(false);
    };

    let entries_guard = entries.read();
    let entries_loading = *entries.state().read() == UseResourceState::Pending;
    let entries_match_tab = entries_guard
        .as_ref()
        .is_some_and(|(surface, _)| *surface == active_tab());
    // Hoist into an owned Vec so the read guard drops before the rsx below.
    let snapshot_entries: Vec<SnapshotEntry> = if entries_loading || !entries_match_tab {
        Vec::new()
    } else {
        entries_guard
            .as_ref()
            .and_then(|(_, result)| result.as_ref().ok())
            .map_or_else(Vec::new, Clone::clone)
    };
    let entries_error = entries_guard
        .as_ref()
        .and_then(|(_, result)| result.as_ref().err())
        .filter(|_| !entries_loading && entries_match_tab)
        .cloned();
    drop(entries_guard);
    let retention_guard = retention.read();
    let current_retention = retention_guard
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .copied();
    let retention_loading = *retention.state().read() == UseResourceState::Pending;
    let now = now_epoch_millis();

    rsx! {
        details { class: "collapse collapse-arrow bg-base-200 rounded-box shadow-md",
            summary { class: "collapse-title text-[0.75rem] font-medium", "Snapshot History" }
            div { class: "collapse-content space-y-4 text-[0.75rem]",
                p { class: "text-base-content/60",
                    "Auto-created when data compacts. Local to this device only."
                    " Each tab restores that surface's records — for Conversations that is the conversation list"
                    " (contacts, titles, previews, unread counts), not message history."
                    " Restore moves this device to the chosen snapshot; the restored state syncs to your other devices."
                }

                // Retention policy
                div { class: "flex items-center gap-2",
                    span { class: "flex-1", "Retention" }
                    select {
                        class: "select select-sm w-32",
                        // The "Retention" text beside it is a plain span, not a
                        // label, so the control needs its own name.
                        "aria-label": "Snapshot retention",
                        disabled: retention_busy() || retention_loading || current_retention.is_none(),
                        value: match current_retention {
                            Some(RetentionPolicy::Days30) => "30",
                            Some(RetentionPolicy::Days90) => "90",
                            Some(RetentionPolicy::Forever) => "forever",
                            None => "",
                        },
                        onchange: move |e: Event<FormData>| {
                            let Some(current) = current_retention else {
                                return;
                            };
                            let policy = match e.value().as_str() {
                                "30" => RetentionPolicy::Days30,
                                "90" => RetentionPolicy::Days90,
                                "forever" => RetentionPolicy::Forever,
                                _ => {
                                    retention_error
                                        .set(Some("Select a valid retention policy.".to_string()));
                                    return;
                                }
                            };
                            if policy == current {
                                return;
                            }
                            if retention_rank(policy) < retention_rank(current) {
                                pending_policy.set(Some(policy));
                            } else {
                                apply_retention_policy(policy, retention_error, retention_busy);
                            }
                        },
                        if current_retention.is_none() {
                            option { value: "", disabled: true,
                                if retention_loading {
                                    "Loading..."
                                } else {
                                    "Unavailable"
                                }
                            }
                        }
                        option { value: "30", "30 days" }
                        option { value: "90", "90 days" }
                        option { value: "forever", "Forever" }
                    }
                    if retention_busy() {
                        span { class: "text-base-content/60", "Saving..." }
                    }
                    div {
                        class: "radial-progress text-xs",
                        role: "progressbar",
                        "aria-valuenow": "{snapshot_entries.len()}",
                        "aria-valuemax": "{MAX_SNAPSHOTS}",
                        style: "--value:{snapshot_entries.len()}; --size:2.2rem; --thickness:3px",
                        "{snapshot_entries.len()}"
                    }
                    span { class: "text-base-content/70", "snapshots used" }
                }
                if let Some(error) = retention_error() {
                    p { class: "text-error text-xs", "{error}" }
                } else if let Some(Err(error)) = retention_guard.as_ref() {
                    p { class: "text-error text-xs", "{error}" }
                }

                // Tabs
                div { class: "tabs tabs-box justify-center",
                    for variant in SyncSurface::ALL {
                        button {
                            class: if active_tab() == variant { "tab tab-active" } else { "tab" },
                            onclick: move |_| active_tab.set(variant),
                            {variant.label()}
                        }
                    }
                }

                // Snapshot timeline (newest first)
                if entries_loading || !entries_match_tab {
                    p { class: "text-base-content/60", "Loading snapshots..." }
                } else if let Some(error) = &entries_error {
                    p { class: "text-error text-xs", "{error}" }
                } else if snapshot_entries.is_empty() {
                    div { class: "text-center text-base-content/70 py-4", "No snapshots yet" }
                } else {
                    ul { class: "timeline timeline-vertical timeline-compact timeline-snap-icon",
                        for entry in snapshot_entries.iter().rev().cloned() {
                            li { key: "{entry.timestamp}",
                                div { class: "timeline-middle",
                                    span { class: "icon-[heroicons--clock] size-3 text-base-content/40" }
                                }
                                div { class: "timeline-start text-base-content/60 text-xs",
                                    div { class: "text-base-content",
                                        "{format_local_ymdhm_from_millis(entry.timestamp)}"
                                    }
                                    div { "{format_relative_time_at(now, entry.timestamp)}" }
                                }
                                div { class: "timeline-end bg-base-200 rounded-field px-3 py-2 ms-4",
                                    div { class: "flex items-center justify-end gap-2",
                                        span { class: "text-base-content/60", "{format_binary_bytes(entry.size)}" }
                                        if restoring()
                                            && pending_restore().map(|(_, timestamp)| timestamp)
                                                == Some(entry.timestamp)
                                        {
                                            span { class: "status status-primary" }
                                        }
                                        button {
                                            id: "snapshot-restore-{entry.timestamp}",
                                            class: "btn btn-sm btn-outline",
                                            disabled: restoring(),
                                            title: "Restore this snapshot",
                                            onclick: move |_| {
                                                restore_success.set(None);
                                                restore_error.set(None);
                                                pending_restore.set(Some((active_tab(), entry.timestamp)));
                                            },
                                            "Restore"
                                        }
                                    }
                                }
                                hr {}
                            }
                        }
                    }
                }

                // Restore feedback (adjacent to the list it describes)
                if let Some(msg) = restore_error() {
                    p { class: "text-error text-xs", "{msg}" }
                } else if let Some(msg) = restore_success() {
                    p { class: "text-success text-xs", "{msg}" }
                }

                // Manual snapshot
                if let Some(msg) = create_error() {
                    div { class: "text-error text-xs text-center", "{msg}" }
                }
                button {
                    id: "snapshots-create-button",
                    class: "btn btn-sm btn-outline btn-block",
                    disabled: creating(),
                    onclick: create_snapshot,
                    if creating() {
                        "Saving snapshot..."
                    } else {
                        "Create Snapshot"
                    }
                }
            }

            RestoreConfirmModal {
                restore_dialog_id: restore_dialog_id.clone(),
                pending_restore,
                restoring,
                on_confirm: move |(surface, timestamp)| {
                    let dialog_id = restore_dialog_id.clone();
                    async move {
                        restore_snapshot(surface, timestamp, dialog_id).await;
                    }
                },
            }
        }
    }
}
