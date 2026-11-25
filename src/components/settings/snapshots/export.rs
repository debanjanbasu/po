//! Export backup section — encrypted backup download/share requests.

use crate::{
    chat::{contact::ContactList, conversation::ConversationIndex, storage::export_all_raw},
    components::{
        settings::shell::SettingsCard,
        shared::{Spinner, SpinnerSize},
    },
    helpers::spawn,
    models::totp::TotpTokenList,
    platform::share::{self, ShareError},
    session::epoch::SessionGuard,
    storage::{
        background_errors::report_nonfatal,
        backup::{self, BackupSection},
    },
    user::{ctx::UserCtx, get_session_crypto},
};
use dioxus::prelude::*;

use super::shared::{SyncSurface, build_identity_section, load_and_pack, pack_surface};

/// Present the prepared backup only for the initiating session.
async fn finish_export(
    guard: SessionGuard,
    mut exporting: Signal<bool>,
    mut export_error: Signal<Option<String>>,
    mut export_feedback: Signal<Option<&'static str>>,
    filename: &str,
    result: Result<Vec<u8>, String>,
) {
    if !guard.is_current() {
        exporting.set(false);
        return;
    }
    // The preparation failure carries its own sentence — a size refusal names
    // the size, the limit and the way out (`storage::backup::ExportError`) — so
    // it is passed through rather than replaced. The Activity entry is the log;
    // the banner is the notice, and a user cannot act on a cause only the log
    // records.
    let outcome: Result<Result<share::DownloadResult, ShareError>, String> = match result {
        Ok(bytes) => {
            Ok(share::download_bytes_async(bytes, filename, "application/octet-stream").await)
        }
        Err(message) => {
            report_nonfatal("settings.backup", "Failed to prepare backup", &message);
            Err(message)
        }
    };
    guard.commit(move || {
        exporting.set(false);
        match outcome {
            Ok(Ok(outcome)) => export_feedback.set(Some(outcome.message())),
            Ok(Err(ShareError::Cancelled)) => export_feedback.set(Some("Export cancelled.")),
            Ok(Err(error)) => export_error.set(Some(error.to_string())),
            Err(message) => export_error.set(Some(message)),
        }
    });
}

/// Export backup section — encrypted backup download/share requests.
#[component]
pub(super) fn ExportSection(
    mut exporting: Signal<bool>,
    mut export_error: Signal<Option<String>>,
) -> Element {
    let mut export_feedback = use_signal(|| None);
    let mut do_export = move |surface: Option<SyncSurface>| {
        if exporting() {
            return;
        }
        let guard = SessionGuard::capture();
        let ctx = UserCtx::try_current();
        exporting.set(true);
        export_error.set(None);
        export_feedback.set(None);

        spawn(async move {
            if !guard.is_current() {
                // Session changed while the spawn was queued: unblock the
                // controls, exactly as `finish_export` does on this path.
                exporting.set(false);
                return;
            }
            let result = async {
                let ctx = ctx.ok_or_else(|| "No active session".to_string())?;
                let secret = get_session_crypto(&ctx.username)
                    .map(|c| c.dht_secret.clone())
                    .ok_or_else(|| "Session secret unavailable".to_string())?;
                let key = ctx.storage_key();
                let sections = if let Some(surface) = surface {
                    vec![pack_surface(surface, key).await?]
                } else {
                    let identity_data = build_identity_section(&ctx)
                        .map_err(|e| format!("Failed to build identity section: {e}"))?;
                    let mut sections = vec![BackupSection {
                        model_id: backup::IDENTITY_SECTION_ID,
                        data: identity_data,
                    }];
                    sections.push(load_and_pack::<TotpTokenList>(key).await?);
                    sections.push(load_and_pack::<ContactList>(key).await?);
                    sections.push(load_and_pack::<ConversationIndex>(key).await?);

                    // Always include this section, even when empty: on restore
                    // its presence marks a complete conversation backup, so
                    // Replace can safely wipe local rows.
                    let convs = export_all_raw(ctx.account_id.as_ref())
                        .await
                        .map_err(|e| format!("Failed to export conversations: {e}"))?;
                    sections.push(BackupSection {
                        model_id: backup::CONVERSATIONS_DATA_SECTION_ID,
                        data: backup::encode_kv_list(&convs),
                    });
                    sections
                };

                backup::export_backup(&secret, sections)
                    .await
                    .map_err(|e| e.to_string())
            }
            .await;

            finish_export(
                guard,
                exporting,
                export_error,
                export_feedback,
                surface.map_or("po-backup.pobk", SyncSurface::filename),
                result,
            )
            .await;
        });
    };

    rsx! {
        SettingsCard {
            li { class: "list-row",
                div { class: "flex-1",
                    div { class: "font-medium", "Export" }
                    div {
                        "Encrypted with your account password. Requires it to restore on another device."
                    }
                }
            }
            if let Some(ref err) = export_error() {
                li { class: "list-row",
                    p { class: "text-error text-xs w-full", role: "alert", "{err}" }
                }
            }
            if let Some(message) = export_feedback() {
                li { class: "list-row",
                    p {
                        class: "text-xs text-base-content/60 w-full",
                        role: "status",
                        "{message}"
                    }
                }
            }
            if cfg!(any(
                target_arch = "wasm32",
                target_os = "ios",
                target_os = "macos",
                target_os = "android"
            )) {
                li { class: "list-row",
                    button {
                        id: "snapshots-export-button",
                        class: "btn btn-sm btn-secondary btn-block",
                        disabled: exporting(),
                        onclick: move |_| do_export(None),
                        if exporting() {
                            Spinner { size: SpinnerSize::Xs }
                        } else {
                            span {
                                class: "icon-[line-md--download-loop]",
                                "aria-hidden": "true",
                            }
                        }
                        "Export Full Backup"
                    }
                }
                li { class: "list-row",
                    div { class: "flex flex-wrap gap-2 w-full",
                        for surface in SyncSurface::ALL {
                            button {
                                key: "{surface.filename()}",
                                class: "btn btn-xs btn-outline flex-1",
                                disabled: exporting(),
                                onclick: move |_| do_export(Some(surface)),
                                "{surface.label()} only"
                            }
                        }
                    }
                }
            } else {
                li { class: "list-row",
                    p { class: "text-xs text-base-content/60 w-full", role: "status",
                        "File export is not available on this device."
                    }
                }
            }
        }
    }
}
