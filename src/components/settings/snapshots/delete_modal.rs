//! Retention downgrade ("delete old backups") confirmation modal, split out
//! of the page module (`mod.rs`).

use dioxus::prelude::*;

use crate::{
    components::shared::{ModalDialog, close_modal_dialog},
    storage::snapshots::RetentionPolicy,
};

/// Retention downgrade confirmation modal. Confirming closes the dialog and
/// hands the pending policy back to the page, which owns applying it.
#[component]
pub(super) fn RetentionConfirmModal(
    retention_dialog_id: String,
    mut pending_policy: Signal<Option<RetentionPolicy>>,
    retention_error: Signal<Option<String>>,
    retention_busy: Signal<bool>,
    on_confirm: Callback<RetentionPolicy>,
) -> Element {
    if pending_policy().is_none() {
        return rsx! {};
    }

    rsx! {
        ModalDialog {
            dialog_id: retention_dialog_id.clone(),
            title: "Reduce backup retention?",
            title_class: "text-warning",
            on_close: move |()| pending_policy.set(None),
            p { class: "py-1",
                "Snapshots older than the new limit will be permanently deleted from this device."
            }
            div { class: "modal-action flex gap-2",
                button {
                    class: "btn btn-sm",
                    autofocus: true,
                    onclick: {
                        let dialog_id = retention_dialog_id.clone();
                        move |_| {
                            close_modal_dialog(&dialog_id);
                            pending_policy.set(None);
                        }
                    },
                    "Cancel"
                }
                button {
                    class: "btn btn-sm btn-warning",
                    disabled: retention_busy(),
                    onclick: move |_| {
                        close_modal_dialog(&retention_dialog_id);
                        if let Some(policy) = pending_policy.take() {
                            on_confirm.call(policy);
                        }
                    },
                    "Delete old backups"
                }
            }
        }
    }
}
