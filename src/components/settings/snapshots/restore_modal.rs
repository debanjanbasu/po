//! Snapshot restore confirmation modal, split out of the page module
//! (`mod.rs`) so the page listing and the modal live apart.

use dioxus::prelude::*;

use crate::components::shared::{ModalDialog, close_modal_dialog, format_local_ymdhm_from_millis};

use super::shared::SyncSurface;

/// Snapshot restore confirmation modal.
#[component]
pub(super) fn RestoreConfirmModal(
    restore_dialog_id: String,
    mut pending_restore: Signal<Option<(SyncSurface, u64)>>,
    restoring: Signal<bool>,
    on_confirm: Callback<(SyncSurface, u64)>,
) -> Element {
    let Some((surface, timestamp)) = pending_restore() else {
        return rsx! {};
    };

    rsx! {
        ModalDialog {
            dialog_id: restore_dialog_id.clone(),
            title: "Restore snapshot?",
            on_close: move |()| pending_restore.set(None),
            p { class: "py-1",
                "Restore {surface.label()} to {format_local_ymdhm_from_millis(timestamp)}?"
                " This restores that surface's records only — for Conversations, the conversation list, not message history."
                " The current state is captured as a new snapshot first; the restored state syncs to your other devices."
            }
            div { class: "modal-action flex gap-2",
                button {
                    class: "btn btn-sm",
                    autofocus: true,
                    onclick: {
                        let dialog_id = restore_dialog_id;
                        move |_| {
                            close_modal_dialog(&dialog_id);
                            pending_restore.set(None);
                        }
                    },
                    "Cancel"
                }
                button {
                    class: "btn btn-sm btn-warning",
                    disabled: restoring(),
                    onclick: move |_| on_confirm.call((surface, timestamp)),
                    "Restore"
                }
            }
        }
    }
}
