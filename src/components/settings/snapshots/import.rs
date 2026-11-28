//! Import/restore components — file picker, password modal, merge/replace modal.

use anyhow::anyhow;
use dioxus::prelude::*;
use zeroize::Zeroizing;

#[cfg(target_os = "ios")]
use crate::platform::share::pick_backup_file;
use crate::{
    components::{
        settings::shell::SettingsCard,
        shared::{
            AutoSubmitGate, ModalDialog, RetryButton, Spinner, SpinnerSize, close_modal_dialog,
        },
    },
    config::backup::MAX_FILE_BYTES,
    helpers::spawn,
    session::epoch::SessionGuard,
    storage::{
        background_errors::report_nonfatal,
        backup::{self, IDENTITY_SECTION_ID, IdentityBackup},
    },
    user::{ctx::UserCtx, validation::validate_password_msg, verify_backup_keypair},
};

use super::{
    plan::{RestoreMode, apply_conversations, apply_model, build_plan, verify_account_binding},
    shared::ImportState,
};

const BACKUP_INPUT_ID: &str = "backup-import-file";

/// Import/restore file picker section.
#[component]
pub(super) fn ImportSection(
    mut file_bytes: Signal<Option<Vec<u8>>>,
    mut password: Signal<String>,
    mut error: Signal<Option<String>>,
) -> Element {
    let mut input_epoch = use_signal(|| 0u64);
    use_effect(move || {
        if file_bytes.read().is_none() {
            input_epoch.with_mut(|n| *n = n.wrapping_add(1));
        }
    });
    let mut loading = use_signal(|| false);
    let on_import_file = move |e: Event<FormData>| {
        if loading() {
            return;
        }
        let files = e.files();
        if files.is_empty() {
            input_epoch.with_mut(|n| *n = n.wrapping_add(1));
            return;
        }
        let guard = SessionGuard::capture();
        loading.set(true);
        error.set(None);
        spawn(async move {
            let Some(first) = files.first() else {
                loading.set(false);
                return;
            };
            if first.size() > MAX_FILE_BYTES {
                // Not "not a valid backup": the file is fine, this platform is
                // smaller than the one that wrote it. Same sentence as
                // `import_backup`'s own refusal, from one constructor.
                error.set(Some(backup::too_large_message(first.size())));
                input_epoch.with_mut(|n| *n = n.wrapping_add(1));
                loading.set(false);
                return;
            }
            let result = first.read_bytes().await;
            guard.commit(|| {
                input_epoch.with_mut(|n| *n = n.wrapping_add(1));
                match result {
                    Ok(bytes) => {
                        file_bytes.set(Some(bytes.into()));
                        password.set(String::new());
                    }
                    Err(detail) => {
                        report_nonfatal("settings.backup", "Failed to read backup file", detail);
                        error.set(Some("Failed to read the selected file".to_string()));
                    }
                }
                loading.set(false);
            });
        });
    };
    let on_pick_ios = move |_| {
        #[cfg(target_os = "ios")]
        {
            if loading() {
                return;
            }
            let guard = SessionGuard::capture();
            loading.set(true);
            error.set(None);
            spawn(async move {
                let result = pick_backup_file(MAX_FILE_BYTES).await;
                guard.commit(|| {
                    match result {
                        Ok(Some(bytes)) => {
                            file_bytes.set(Some(bytes));
                            password.set(String::new());
                        }
                        Ok(None) => {}
                        Err(err) => error.set(Some(err.to_string())),
                    }
                    loading.set(false);
                });
            });
        }
    };

    let show_import = cfg!(target_arch = "wasm32")
        || cfg!(target_os = "macos")
        || cfg!(target_os = "linux")
        || cfg!(target_os = "windows")
        || cfg!(target_os = "ios");

    // iOS's document picker can hide files with unrecognized extensions when
    // an accept filter is set — allow everything there; import validates the
    // file's magic bytes anyway.
    let accept_filter = if cfg!(target_os = "ios") {
        None
    } else {
        Some(".pobk")
    };

    rsx! {
        SettingsCard {
            li { class: "list-row",
                div { class: "flex-1",
                    div { class: "font-medium", "Restore" }
                    div {
                        "Import a backup file. You\u{2019}ll enter your password and choose to replace or merge."
                    }
                }
            }
            if cfg!(target_os = "ios") {
                button {
                    class: "btn btn-sm btn-outline btn-block",
                    r#type: "button",
                    disabled: loading(),
                    aria_label: "Import backup file",
                    onclick: on_pick_ios,
                    span { class: "icon-[line-md--upload-loop]", "aria-hidden": "true" }
                    if loading() { "Reading backup..." } else { "Import Backup File" }
                }
            } else if show_import {
                label { class: "btn btn-sm btn-outline btn-block focus-within:outline focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-primary",
                    input {
                        key: "{input_epoch}",
                        id: BACKUP_INPUT_ID,
                        r#type: "file",
                        accept: accept_filter,
                        class: "sr-only",
                        aria_label: "Import backup file",
                        disabled: loading(),
                        oncancel: move |_| input_epoch.with_mut(|n| *n = n.wrapping_add(1)),
                        onchange: on_import_file,
                    }
                    span { class: "icon-[line-md--upload-loop]", "aria-hidden": "true" }
                    if loading() { "Reading backup..." } else { "Import Backup File" }
                }
            } else {
                li { class: "list-row",
                    p { class: "text-xs text-base-content/60 w-full", role: "status",
                        "File import is not available on this device."
                    }
                }
            }
            if let Some(message) = error() {
                li { class: "list-row",
                    p { class: "text-error text-xs", "{message}" }
                }
            }
        }
    }
}

/// Import password entry modal.
#[component]
pub(super) fn ImportPasswordModal(import_dialog_id: String, mut state: ImportState) -> Element {
    let mut attempt = use_signal(|| 0u64);
    let password_gate = AutoSubmitGate::<String>::use_gate();

    let mut start_decrypt = move |password_str: String| {
        if (state.importing)() {
            return;
        }
        let password = Zeroizing::new(password_str);
        if password.is_empty() {
            state.error.set(Some("Password is required".to_string()));
            return;
        }
        let Some(file_bytes) = state.file_bytes.take() else {
            state.error.set(Some("No file loaded".to_string()));
            return;
        };
        state.importing.set(true);
        state.error.set(None);
        let generation = attempt.peek().wrapping_add(1);
        attempt.set(generation);
        let guard = SessionGuard::capture();

        spawn(async move {
            let result = match UserCtx::try_current() {
                Some(ctx) => {
                    match backup::import_backup(&password, &ctx.account_id, &file_bytes).await {
                        Ok(sections) => {
                            let identity_check = if let Some((_, data)) =
                                sections.iter().find(|(id, _)| id == IDENTITY_SECTION_ID)
                            {
                                match IdentityBackup::decode(data) {
                                    Ok(identity) => {
                                        verify_backup_keypair(
                                            &ctx.username,
                                            &password,
                                            &identity.account_id,
                                            &identity.keypair_nonce,
                                            &identity.keypair_ciphertext,
                                        )
                                        .await
                                    }
                                    Err(e) => Err(e),
                                }
                            } else {
                                Ok(())
                            };
                            identity_check.map(|()| sections)
                        }
                        Err(e) => Err(e),
                    }
                }
                None => Err(anyhow!("No active account")),
            };
            if *attempt.peek() != generation || !guard.is_current() {
                return;
            }
            if state.file_bytes.peek().is_some() {
                state.importing.set(false);
                return;
            }
            match result {
                Ok(sections) => {
                    state.sections.set(Some(sections));
                }
                Err(e) => {
                    let msg = e
                        .downcast_ref::<backup::ImportError>()
                        .map_or_else(|| format!("Import failed: {e}"), ToString::to_string);
                    state.error.set(Some(msg));
                    state.file_bytes.set(Some(file_bytes));
                }
            }
            state.importing.set(false);
        });
    };

    use_effect(move || {
        if (state.importing)()
            || state.file_bytes.read().is_none()
            || state.sections.read().is_some()
        {
            return;
        }
        let password = (state.password)();
        if validate_password_msg(&password).is_some() {
            return;
        }
        if !password_gate.claim(password.clone()) {
            return;
        }
        start_decrypt(password);
    });

    if (state.file_bytes.read().is_none() && !(state.importing)())
        || state.sections.read().is_some()
    {
        return rsx! {};
    }

    let close_and_reset = {
        let dialog_id = import_dialog_id.clone();
        Callback::new(move |()| {
            attempt.with_mut(|value| *value = value.wrapping_add(1));
            close_modal_dialog(&dialog_id);
            password_gate.clear();
            state.importing.set(false);
            state.file_bytes.set(None);
            state.password.set(String::new());
            state.error.set(None);
        })
    };

    rsx! {
        ModalDialog {
            dialog_id: import_dialog_id,
            title: "Enter backup password",
            show_close_button: true,
            on_close: move |()| close_and_reset.call(()),
            div { class: "space-y-3",
                label { class: "input input-sm w-full",
                    input {
                        r#type: "password",
                        placeholder: "Password used when exporting",
                        "aria-label": "Backup password",
                        disabled: (state.importing)(),
                        value: "{state.password}",
                        oninput: move |ev| {
                            let next = ev.value();
                            if password_gate.peek().as_deref() != Some(next.as_str()) {
                                password_gate.clear();
                            }
                            state.error.set(None);
                            state.password.set(next);
                        },
                        onkeydown: move |ev| {
                            if ev.key() == Key::Named(NamedKey::Enter) && !(state.importing)() {
                                let password = state.password.read().clone();
                                if !password.is_empty() {
                                    password_gate.claim(password.clone());
                                    start_decrypt(password);
                                }
                            }
                        },
                    }
                }
                if (state.importing)() {
                    div {
                        class: "flex items-center justify-center gap-2 text-xs text-base-content/70",
                        role: "status",
                        Spinner { size: SpinnerSize::Xs }
                        span { "Decrypting…" }
                    }
                }
                if let Some(err) = &(state.error)() {
                    p { class: "text-error text-xs", "{err}" }
                    RetryButton {
                        class: "btn-xs",
                        onclick: move |()| {
                            let password = state.password.read().clone();
                            if !password.is_empty() {
                                password_gate.claim(password.clone());
                                start_decrypt(password);
                            }
                        },
                    }
                }
            }
        }
    }
}

/// Import merge/replace choice modal.
#[component]
pub(super) fn ImportMergeModal(import_merge_dialog_id: String, mut state: ImportState) -> Element {
    let mut applying = use_signal(|| false);
    if state.sections.read().is_none() && !applying() {
        return rsx! {};
    }

    let mut apply_restore = move |mode: RestoreMode| {
        if applying() {
            return;
        }
        let Some(ctx) = UserCtx::try_current() else {
            state.error.set(Some("No active session".to_string()));
            return;
        };
        let Some(sections) = state.sections.take() else {
            return;
        };
        applying.set(true);
        state.error.set(None);
        spawn(async move {
            // Fail closed before anything mutates: every section must decode
            // and the backup must belong to this account.
            let plan = match build_plan(&sections)
                .and_then(|plan| verify_account_binding(&ctx, &plan).map(|()| plan))
            {
                Ok(plan) => plan,
                Err(e) => {
                    state.error.set(Some(e.to_string()));
                    state.file_bytes.set(None);
                    state.password.set(String::new());
                    applying.set(false);
                    return;
                }
            };

            let mut errors = Vec::new();
            // Conversations next (index + data as one atomic unit), then the
            // small independent models.
            if let Err(e) = apply_conversations(
                &ctx,
                plan.index.as_ref(),
                plan.conversations.as_deref(),
                mode,
            )
            .await
            {
                errors.push(format!("conversations: {e}"));
            }
            if let Some(totp) = &plan.totp
                && let Err(e) = apply_model(&ctx, totp, mode).await
            {
                errors.push(format!("totp: {e}"));
            }
            if let Some(contacts) = &plan.contacts
                && let Err(e) = apply_model(&ctx, contacts, mode).await
            {
                errors.push(format!("contacts: {e}"));
            }

            if errors.is_empty() {
                state.success.set(true);
            } else {
                let msg = format!(
                    "Partial restore — {} error(s): {}",
                    errors.len(),
                    errors.join("; ")
                );
                report_nonfatal("settings.backup", "Restore had errors", &msg);
                state.error.set(Some(msg));
            }

            state.file_bytes.set(None);
            state.password.set(String::new());
            applying.set(false);
        });
    };
    let close_and_reset = {
        let dialog_id = import_merge_dialog_id.clone();
        Callback::new(move |()| {
            if applying() {
                return;
            }
            close_modal_dialog(&dialog_id);
            state.sections.set(None);
            state.file_bytes.set(None);
            state.password.set(String::new());
        })
    };

    rsx! {
        ModalDialog {
            dialog_id: import_merge_dialog_id,
            title: "Restore backup",
            close_disabled: applying(),
            on_close: move |()| close_and_reset.call(()),
            div { class: "space-y-2",
                p { class: "text-sm", "How should the restored data be applied?" }
                div { class: "text-xs text-base-content/60 space-y-1",
                    p {
                        strong { "Replace" }
                        " — Overwrites current data entirely with the backup."
                    }
                    p {
                        strong { "Merge" }
                        " — Adds missing backup items and keeps current values for existing items."
                    }
                }
                if let Some(err) = &(state.error)() {
                    p { class: "text-error text-xs", "{err}" }
                }
                if applying() {
                    p { class: "text-xs text-base-content/60", "Restoring local data..." }
                }
            }
            div { class: "modal-action flex gap-2",
                button {
                    class: "btn btn-sm",
                    disabled: applying(),
                    onclick: move |_| close_and_reset.call(()),
                    "Cancel"
                }
                button {
                    id: "backup-restore-replace",
                    class: "btn btn-sm btn-warning",
                    disabled: applying(),
                    onclick: move |_| apply_restore(RestoreMode::Replace),
                    "Replace"
                }
                button {
                    id: "backup-restore-merge",
                    class: "btn btn-sm btn-primary",
                    disabled: applying(),
                    onclick: move |_| apply_restore(RestoreMode::Merge),
                    "Merge"
                }
            }
        }
    }
}
