//! TOTP (Time-based One-Time Password) page.

use anyhow::anyhow;
use dioxus::prelude::*;

use crate::{
    components::shared::{
        CenteredPage, FeedbackKind, FormFeedback, ModalDialog, QrAcquireButtons, Spinner,
        SpinnerSize, close_modal_dialog, use_modal_dialog_id, use_transient_message,
    },
    config::ui::{SHARE_FEEDBACK_MS, SUCCESS_TOAST_MS as TOAST_AUTO_DISMISS_MS},
    helpers::{
        async_utils::{now_epoch_millis, now_epoch_secs, sleep_ms},
        spawn,
    },
    models::totp::{TotpTokenData, TotpTokenList, parse_totp_uri},
    platform::share::{self, ShareError},
    session::epoch::SessionGuard,
    storage::{
        background_errors::report_nonfatal,
        store_trait::{load_local, use_model_revision},
    },
    sync::mutate::{MutationResult, mutate},
    user::ctx::UserCtx,
};

/// Countdown updates every second for smooth UI display.
const COUNTDOWN_UPDATE_INTERVAL_MS: u64 = 1000;

fn use_totp_second_tick(enabled: Memo<bool>) -> Signal<u64> {
    let mut tick = use_signal(now_epoch_secs);

    // `use_resource` tracks the memo reactively, cancelling the prior loop when
    // tokens disappear and starting a fresh mounted-scoped loop when they
    // arrive later.
    use_resource(move || {
        let enabled = enabled();
        async move {
            tick.set(now_epoch_secs());
            if !enabled {
                return;
            }
            loop {
                let wait_ms = COUNTDOWN_UPDATE_INTERVAL_MS
                    .saturating_sub(now_epoch_millis() % COUNTDOWN_UPDATE_INTERVAL_MS)
                    .max(1);
                sleep_ms(wait_ms).await;
                tick.set(now_epoch_secs());
            }
        }
    });

    tick
}

fn shared_period(tokens: &[TotpTokenData]) -> Option<u32> {
    let period = tokens.first()?.period;
    (period > 0 && tokens.iter().all(|token| token.period == period)).then_some(period)
}

/// TOTP management page.
#[component]
pub fn Totp() -> Element {
    let Some(ctx) = UserCtx::try_current() else {
        return rsx! {
            CenteredPage { contained: true,
                p { "Sign in to manage your authenticator tokens." }
            }
        };
    };
    let epoch = SessionGuard::capture().epoch();
    rsx! {
        AccountTotp { key: "{ctx.account_id}:{epoch}" }
    }
}

#[component]
fn AddTokenCard(
    user_ctx: Memo<Option<UserCtx>>,
    actions_disabled: Memo<bool>,
    mut mutating: Signal<bool>,
) -> Element {
    let mut scanned_uri = use_signal(String::new);
    let (success_message, mut success_set) = use_transient_message(TOAST_AUTO_DISMISS_MS);
    let mut action_error = use_signal(|| None::<String>);
    let mut last_attempted_uri = use_signal(|| None::<String>);
    let parsed_uri = use_memo(move || {
        let uri = scanned_uri.read();
        (!uri.is_empty()).then(|| parse_totp_uri(&uri).map_err(|e| e.to_string()))
    });
    let validation_error = use_memo(move || match parsed_uri.read().as_ref() {
        Some(Err(err)) => Some(err.clone()),
        _ => None,
    });
    let uri_is_valid = use_memo(move || matches!(parsed_uri.read().as_ref(), Some(Ok(_))));

    let add_token = move |uri: String, guard: SessionGuard| {
        let ctx = user_ctx();
        async move {
            let ctx = ctx.ok_or_else(|| anyhow!("Not logged in"))?;
            mutate::<TotpTokenList, _>(&ctx, |list| {
                if !guard.is_current() {
                    return Ok(MutationResult::unchanged(false));
                }
                if list.add_from_uri(&uri)? {
                    Ok(MutationResult::modified(true))
                } else {
                    Ok(MutationResult::unchanged(false))
                }
            })
            .await
        }
    };

    use_effect(move || {
        if !uri_is_valid() || actions_disabled() || mutating() {
            return;
        }
        let uri = scanned_uri();
        if last_attempted_uri().as_deref() == Some(uri.as_str()) {
            return;
        }
        last_attempted_uri.set(Some(uri.clone()));
        mutating.set(true);
        action_error.set(None);
        success_set.clear();
        let guard = SessionGuard::capture();
        spawn(async move {
            let result = add_token(uri, guard).await;
            guard.commit(|| {
                mutating.set(false);
                match result {
                    Ok(true) => {
                        scanned_uri.set(String::new());
                        last_attempted_uri.set(None);
                        success_set.set("Token added.");
                    }
                    Ok(false) => action_error.set(Some(
                        "A token for this issuer and account already exists.".to_string(),
                    )),
                    Err(error) => {
                        report_nonfatal("components.totp", "Failed to add token", &error);
                        action_error.set(Some(
                            "Unable to finish adding or syncing this token. Check the token list before retrying."
                                .to_string(),
                        ));
                    }
                }
            });
        });
    });

    rsx! {
        div { class: "card bg-base-200 mb-6 shadow-sm",
            div { class: "card-body p-4 gap-4",
                if let Some(msg) = success_message() {
                    FormFeedback { kind: FeedbackKind::Success, message: msg, compact: true }
                }

                div {
                    class: if scanned_uri.read().is_empty() || uri_is_valid() { "join w-full" } else { "join w-full validator-error" },
                    input {
                        id: "totp-uri-input",
                        r#type: "text",
                        "aria-label": "Authenticator URI",
                        class: "input input-sm validator join-item w-full",
                        placeholder: "otpauth://totp/… (HOTP not supported)",
                        value: scanned_uri,
                        disabled: mutating(),
                        oninput: move |ev| {
                            let next = ev.value();
                            if last_attempted_uri.peek().as_deref() != Some(next.as_str()) {
                                last_attempted_uri.set(None);
                            }
                            action_error.set(None);
                            scanned_uri.set(next);
                        },
                    }
                    QrAcquireButtons {
                        disabled: mutating(),
                        on_result: move |uri: String| {
                            if !*mutating.peek() {
                                if last_attempted_uri.peek().as_deref() != Some(uri.as_str()) {
                                    last_attempted_uri.set(None);
                                }
                                action_error.set(None);
                                scanned_uri.set(uri);
                            }
                        },
                        on_error: move |error: String| action_error.set(Some(error)),
                        scan_aria_label: "Scan TOTP QR code",
                        upload_aria_label: "Upload TOTP QR image",
                    }
                }
                if let Some(err) = action_error.read().as_ref() {
                    p { class: "validator-hint text-error", "{err}" }
                } else if let Some(err) = validation_error.read().as_ref() {
                    p { class: "validator-hint text-error", "{err}" }
                } else {
                    p { class: "validator-hint",
                        "Paste a URI, scan a QR code, or upload a QR image"
                    }
                }

                if mutating() {
                    div {
                        class: "flex items-center justify-center gap-2 text-xs text-base-content/70",
                        role: "status",
                        Spinner { size: SpinnerSize::Xs }
                        span { "Adding..." }
                    }
                }
            }
        }
    }
}

#[component]
fn AccountTotp() -> Element {
    let mut remove_error = use_signal(|| None::<&'static str>);
    let mut mutating = use_signal(|| false);
    let user_ctx = use_memo(UserCtx::try_current);
    let model_revision = use_model_revision::<TotpTokenList>();

    // This account/session-keyed resource is the only token-list state.
    let mut token_load = use_resource(move || {
        let ctx = user_ctx();
        let _ = model_revision();
        let guard = SessionGuard::capture();
        async move {
            let Some(ctx) = ctx else {
                return Err("Sign in to manage your authenticator tokens.");
            };
            match load_local::<TotpTokenList>(ctx.storage_key()).await {
                Ok(list) => {
                    let mut tokens: Vec<_> = list.tokens.into_values().collect();
                    tokens.sort_unstable_by(|a, b| {
                        (&a.issuer, &a.account_name).cmp(&(&b.issuer, &b.account_name))
                    });
                    Ok(tokens)
                }
                Err(error) => {
                    guard.commit(|| {
                        report_nonfatal("components.totp", "Failed to load tokens", &error);
                    });
                    Err(
                        "Unable to load saved tokens. Try again, or update po if they were saved by a newer version.",
                    )
                }
            }
        }
    });
    let loading = use_memo(move || *token_load.state().read() == UseResourceState::Pending);
    let load_error = use_memo(move || match token_load.read().as_ref() {
        Some(Err(error)) => Some(*error),
        _ => None,
    });
    let countdown_enabled = use_memo(
        move || matches!(token_load.read().as_ref(), Some(Ok(tokens)) if !tokens.is_empty()),
    );
    let tick = use_totp_second_tick(countdown_enabled);
    let shared_window = use_memo(move || tick() / 30);
    let actions_disabled =
        use_memo(move || mutating() || loading() || load_error().is_some() || user_ctx().is_none());

    let request_remove_token = move |key: String, guard: SessionGuard| {
        let ctx = user_ctx();
        async move {
            let ctx = ctx.ok_or_else(|| anyhow!("Not logged in"))?;
            mutate::<TotpTokenList, _>(&ctx, |list| {
                if !guard.is_current() || !list.remove_token(&key) {
                    return Ok(MutationResult::unchanged(()));
                }
                Ok(MutationResult::modified(()))
            })
            .await
        }
    };

    // Removal is the one action in this view that cannot be recovered in-app:
    // the token secret is deleted and there is no undo. It therefore goes
    // through the same ask/cancel dialog shape as account deletion
    // (`settings/account.rs`) instead of firing on first click.
    let remove_dialog_id = use_modal_dialog_id("totp-remove-dialog");
    let mut pending_remove: Signal<Option<(String, String)>> = use_signal(|| None);
    let confirm_remove = move |key: String| {
        if *mutating.peek() || actions_disabled() {
            return;
        }
        let guard = SessionGuard::capture();
        mutating.set(true);
        remove_error.set(None);
        spawn(async move {
            let result = request_remove_token(key, guard).await;
            guard.commit(|| {
                mutating.set(false);
                if let Err(error) = result {
                    report_nonfatal("components.totp", "Failed to remove token", &error);
                    remove_error.set(Some(
                        "Unable to finish removing or syncing this token. Check the token list before retrying.",
                    ));
                }
            });
        });
    };

    rsx! {
        CenteredPage { contained: true,
            h1 { class: "text-center mb-6", "Authenticator" }

            AddTokenCard {
                user_ctx,
                actions_disabled,
                mutating,
            }

            if mutating() || loading() {
                p { class: "text-xs text-base-content/60", role: "status",
                    if mutating() { "Updating saved tokens..." } else { "Loading tokens..." }
                }
            }
            if let Some(error) = load_error() {
                p { class: "text-error", role: "alert", "{error}" }
                button {
                    class: "btn btn-sm btn-ghost",
                    disabled: loading() || mutating(),
                    onclick: move |_| token_load.restart(),
                    "Retry loading tokens"
                }
            }
            if let Some(error) = remove_error() {
                p { class: "text-error", role: "alert", "{error}" }
            }

            if let Some(Ok(tokens)) = token_load.read().as_ref() {
                if tokens.is_empty() {
                    if !loading() {
                        div { id: "totp-empty-state", class: "text-center py-8 text-base-content/60",
                            p { "No TOTP tokens added yet." }
                            p { class: "mt-2", "Scan a QR code to get started." }
                        }
                    }
                } else {
                    {
                        let shared_period = shared_period(tokens.as_slice());
                        rsx! {
                            div { class: "space-y-4",
                                TotpHeader {
                                    shared_period,
                                    tick,
                                }
                                div { class: "space-y-3",
                                    {tokens.iter().enumerate().map(|(index, token)| {
                                        let key = token.map_key();
                                        let issuer = token.issuer.clone();
                                        rsx! {
                                            TokenCard {
                                                key: "{key}",
                                                index,
                                                token: token.clone(),
                                                tick,
                                                shared_window,
                                                show_timing: shared_period.is_none(),
                                                mutation_pending: actions_disabled(),
                                                on_remove: move |()| {
                                                    if *mutating.peek() || actions_disabled() {
                                                        return;
                                                    }
                                                    remove_error.set(None);
                                                    pending_remove.set(Some((key.clone(), issuer.clone())));
                                                },
                                            }
                                        }
                                    })}
                                }
                            }
                        }
                    }
                }
            }
        }

        if pending_remove().is_some() {
            RemoveTokenConfirm {
                dialog_id: remove_dialog_id,
                pending: pending_remove,
                mutating: mutating,
                on_confirm: confirm_remove,
            }
        }
    }
}

/// Ask/cancel dialog for token removal, mirroring the account-deletion modal in
/// `settings/account.rs`. `pending` carries `(key, issuer)`; confirming takes
/// the key out so a double-fire cannot remove twice.
#[component]
fn RemoveTokenConfirm(
    dialog_id: String,
    mut pending: Signal<Option<(String, String)>>,
    mutating: Signal<bool>,
    on_confirm: EventHandler<String>,
) -> Element {
    let Some((_, issuer)) = pending() else {
        return rsx! {};
    };
    rsx! {
        ModalDialog {
            dialog_id: dialog_id.clone(),
            title: "Remove token?",
            title_class: "text-error",
            on_close: move |()| pending.set(None),

            p { class: "py-1",
                "The {issuer} code will stop working on this device and this cannot be undone. "
                "Any account that uses it for sign-in verification has to be set up again."
            }
            div { class: "modal-action flex gap-2",
                button {
                    class: "btn btn-sm",
                    autofocus: true,
                    onclick: {
                        let dialog_id = dialog_id.clone();
                        move |_| {
                            close_modal_dialog(&dialog_id);
                            pending.set(None);
                        }
                    },
                    "Cancel"
                }
                button {
                    r#type: "button",
                    class: "btn btn-sm btn-error",
                    disabled: mutating(),
                    onclick: move |_| {
                        close_modal_dialog(&dialog_id);
                        if let Some((key, _)) = pending.take() {
                            on_confirm.call(key);
                        }
                    },
                    "Remove token"
                }
            }
        }
    }
}

#[component]
fn TotpHeader(shared_period: Option<u32>, tick: Signal<u64>) -> Element {
    let remaining = shared_period.map(|period| {
        let period_u64 = u64::from(period.max(1));
        let remaining = period_u64 - (tick() % period_u64);
        (period, remaining)
    });
    rsx! {
        div { class: "flex items-center gap-3",
            h2 { "Your Tokens" }
            div { class: "flex-1" }
            if let Some((_, remaining)) = remaining {
                div { class: "flex items-center gap-2 opacity-60",
                    div { class: "countdown",
                        span { style: "--value:{remaining};" }
                    }
                    span { "s" }
                }
            }
        }
        if let Some((period, remaining)) = remaining {
            progress {
                class: "progress progress-secondary w-full",
                value: "{remaining}",
                max: "{period}",
            }
        }
    }
}

#[component]
fn TokenCard(
    /// Position in the rendered (issuer, account)-sorted list; only used for stable DOM ids.
    index: usize,
    token: TotpTokenData,
    tick: Signal<u64>,
    shared_window: Memo<u64>,
    show_timing: bool,
    mutation_pending: bool,
    on_remove: EventHandler<()>,
) -> Element {
    let (copy_feedback, mut copy_feedback_set) = use_transient_message(SHARE_FEEDBACK_MS);
    let mut copy_error = use_signal(|| None::<ShareError>);
    let mut copying = use_signal(|| false);
    let token_memo = use_memo(use_reactive((&token,), |(token,)| token));
    let window = use_memo(move || {
        let period = token_memo.read().period;
        if !show_timing && period == 30 {
            shared_window()
        } else {
            tick() / u64::from(period.max(1))
        }
    });
    let code = use_memo(move || {
        let token = token_memo.read();
        let period_secs = u64::from(token.period.max(1));
        token.calculate_token(window() * period_secs).ok()
    });
    let code_value = code();
    let show_timing = show_timing && code_value.is_some() && token.period > 0;
    rsx! {
        div { class: "aura aura-glow w-full",
            div { class: "card bg-base-200 shadow",
                div { class: "card-body p-3",
                    div { class: "flex items-center gap-3",
                        div { class: "flex-1 min-w-0",
                            h3 { class: "truncate", "{token.issuer}" }
                            p { class: "opacity-60 truncate", "{token.account_name}" }
                        }
                        if let Some(code_str) = &code_value {
                            button {
                                class: "btn btn-sm btn-ghost gap-1 font-mono",
                                "aria-label": "Copy code",
                                disabled: copying(),
                                onclick: move |_| {
                                    if *copying.peek() {
                                        return;
                                    }
                                    copying.set(true);
                                    copy_error.set(None);
                                    copy_feedback_set.clear();
                                    let guard = SessionGuard::capture();
                                    let Some(code) = code.peek().clone() else {
                                        return;
                                    };
                                    spawn(async move {
                                        let result = share::copy_to_clipboard(&code).await;
                                        guard.commit(|| {
                                            copying.set(false);
                                            match result {
                                                Ok(()) => copy_feedback_set.set("Code copied"),
                                                Err(error) => copy_error.set(Some(error)),
                                            }
                                        });
                                    });
                                },
                                span { id: "totp-code-{index}", "{code_str}" }
                                span { class: "icon-[heroicons--clipboard-document] text-base-content/60" }
                            }
                        } else {
                            span { class: "badge badge-error badge-outline", "Unavailable" }
                        }
                        button {
                            id: "totp-remove-{index}",
                            class: "btn btn-sm btn-ghost btn-circle opacity-60 hover:opacity-100",
                            "aria-label": "Remove token",
                            disabled: mutation_pending,
                            onclick: move |_| on_remove.call(()),
                            span { class: "icon-[heroicons--trash]" }
                        }
                    }
                    if copying() {
                        p { class: "text-base-content/60 text-xs", role: "status", "Copying..." }
                    } else if let Some(message) = copy_feedback() {
                        p { class: "text-success text-xs", role: "status", "{message}" }
                    }
                    if let Some(error) = copy_error() {
                        p {
                            class: if error == ShareError::Cancelled {
                                "text-base-content/60 text-xs"
                            } else {
                                "text-error text-xs"
                            },
                            role: if error == ShareError::Cancelled { "status" } else { "alert" },
                            "{error}"
                        }
                    }
                    if code_value.is_none() {
                        p { class: "mt-3 text-sm text-error",
                            "Unable to generate a code for this token right now. Remove it and add it again."
                        }
                    }
                    if show_timing {
                        TokenCountdown { period: token.period, tick }
                    }
                }
            }
        }
    }
}

// Reads tick itself so only this small subtree re-renders on each 1s tick.
#[component]
fn TokenCountdown(period: u32, tick: Signal<u64>) -> Element {
    // Defensive: period 0 would make the modulo below panic.
    if period == 0 {
        return rsx! {};
    }
    // Match TotpTokenData::seconds_remaining: at a boundary the full period remains.
    let remaining = u64::from(period) - (tick() % u64::from(period));
    rsx! {
        div { class: "mt-3 space-y-1",
            div { class: "flex items-center gap-2 text-xs text-base-content/60",
                div { class: "countdown",
                    span { style: "--value:{remaining};" }
                }
                span { "s remaining" }
            }
            progress {
                class: "progress progress-secondary w-full h-1.5",
                value: "{remaining}",
                max: "{period}",
            }
        }
    }
}
