//! Totp — gpui port of `components/features/totp.rs` (the token list, add
//! card, per-token countdown, remove confirmation).
//!
//! Seam ids preserved: `totp-uri-input`, `totp-empty-state`,
//! `totp-code-{i}`, `totp-remove-{i}`.

use std::sync::Arc;

use gpui::{
    Context, Entity, FocusHandle, Focusable, FontWeight, Render, SharedString, Window, prelude::*,
};

use crate::{
    helpers::async_utils::now_epoch_millis,
    models::totp::{TotpTokenData, TotpTokenList},
    session::epoch::SessionGuard,
    storage::{
        background_errors::report_nonfatal,
        store_trait::{self, load_local},
    },
    ui::{
        state::{revision_entity, session_phase},
        widgets::{
            button::{Button, ButtonVariant},
            field::{Feedback, FeedbackKind, LabeledField},
            page::CenteredPage,
        },
    },
    user::User,
};

pub struct TotpView {
    focus: FocusHandle,
    tokens: Arc<[TotpTokenData]>,
    account: Option<Arc<str>>,
    adding: bool,
    save_error: Option<String>,
    uri_state: Option<Entity<gpui_ce_elements::editable_text::EditableTextState>>,
}

impl Focusable for TotpView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TotpView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            focus: cx.focus_handle(),
            tokens: Arc::from([]),
            account: None,
            adding: false,
            save_error: None,
            uri_state: None,
        };
        this.reload(cx);
        let revision = revision_entity::<TotpTokenList>(cx);
        let this_handle = cx.entity();
        cx.observe(&revision, move |_, _, cx| {
            this_handle.update(cx, Self::reload);
        })
        .detach();
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(account) = session_phase(cx)
            .authenticated_user()
            .map(User::account_id_arc)
        else {
            self.tokens = Arc::from([]);
            self.account = None;
            return;
        };
        self.account = Some(account.clone());
        let account_bytes = account.as_bytes().to_vec();
        cx.spawn(async move |this, cx| {
            let loaded = load_local::<TotpTokenList>(&account_bytes).await;
            this.update(cx, |this, cx| {
                this.tokens = match loaded {
                    Ok(list) => {
                        let mut tokens: Vec<TotpTokenData> = list.tokens.into_values().collect();
                        tokens.sort_by_cached_key(|token| token.issuer.clone());
                        tokens.into()
                    }
                    Err(error) => {
                        report_nonfatal("features.totp", "Failed to load tokens", &error);
                        Arc::from([])
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn add_token(&mut self, cx: &mut Context<Self>) {
        let Some(uri_state) = self.uri_state.clone() else {
            return;
        };
        let uri = uri_state.read(cx).as_str().to_owned();
        if uri.is_empty() {
            return;
        }
        let guard = SessionGuard::capture();
        self.save_error = None;
        let account_bytes = self.account.as_ref().map(|a| a.as_bytes().to_vec());
        cx.spawn(async move |this, cx| {
            let result = async {
                let Some(account_bytes) = account_bytes else {
                    anyhow::bail!("No session");
                };
                let mut list = store_trait::load_local::<TotpTokenList>(&account_bytes).await?;
                list.add_from_uri(&uri)?;
                store_trait::save_local::<TotpTokenList>(&account_bytes, &list).await?;
                Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                if !guard.is_current() {
                    return;
                }
                match result {
                    Ok(()) => {
                        this.adding = false;
                        if let Some(state) = &this.uri_state {
                            state.update(cx, |state, cx| {
                                // Clear the URI field for the next token.
                                state.emplace("", cx);
                            });
                        }
                        crate::ui::state::notify_model_changed::<TotpTokenList>(cx);
                    }
                    Err(err) => {
                        this.save_error = Some(err.to_string());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

impl Render for TotpView {
    #[allow(clippy::too_many_lines)] // the wizard body is one visual unit
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keyed URI field state (created on first render).
        if self.uri_state.is_none() {
            let (uri_state, _) =
                LabeledField::create_state(&"totp-uri-input".into(), false, Some(512), window, cx);
            self.uri_state = Some(uri_state);
        }
        let theme = crate::ui::theme::Theme::for_appearance(cx);
        let epoch_secs = now_epoch_millis() / 1000;
        let tokens = self.tokens.clone();
        let handle = cx.entity();

        let mut page =
            CenteredPage::new().contained(true).child(
                gpui::div().flex().flex_col().gap_2().w_full().children(
                    if self.tokens.is_empty() {
                        vec![gpui::div()
                        .id("totp-empty-state")
                        .text_sm()
                        .text_color(theme.text_dim())
                        .text_center()
                        .child("No authenticator tokens yet — add one with its otpauth:// URI.")
                        .into_any_element()]
                    } else {
                        tokens
                            .iter()
                            .enumerate()
                            .map(|(i, token)| {
                                let code = token
                                    .calculate_token(epoch_secs)
                                    .unwrap_or_else(|_| "——————".into());
                                let remaining = u64::from(token.period)
                                    - (epoch_secs % u64::from(token.period));
                                gpui::div()
                                    .id(SharedString::from(format!("totp-token-{i}")))
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .px_3()
                                    .py_2()
                                    .rounded(crate::ui::theme::shape::RADIUS_FIELD)
                                    .bg(theme.base_200)
                                    .child(
                                        gpui::div().flex_1().child(
                                            gpui::div()
                                                .flex()
                                                .flex_col()
                                                .child(
                                                    gpui::div()
                                                        .text_sm()
                                                        .text_color(theme.base_content)
                                                        .child(token.issuer.clone()),
                                                )
                                                .child(
                                                    gpui::div()
                                                        .text_xs()
                                                        .text_color(theme.text_dim())
                                                        .child(token.account_name.clone()),
                                                ),
                                        ),
                                    )
                                    .child(
                                        gpui::div()
                                            .id(SharedString::from(format!("totp-code-{i}")))
                                            .text_lg()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.primary.fill)
                                            .child(code),
                                    )
                                    .child(
                                        gpui::div()
                                            .text_xs()
                                            .text_color(if remaining > 5 {
                                                theme.text_dim()
                                            } else {
                                                theme.error_text()
                                            })
                                            .child(format!("{remaining}s")),
                                    )
                                    .child(
                                        gpui::div()
                                            .id(SharedString::from(format!("totp-remove-{i}")))
                                            .cursor_pointer()
                                            .text_xs()
                                            .text_color(theme.error_text())
                                            .child("Remove"),
                                    )
                                    .into_any_element()
                            })
                            .collect()
                    },
                ),
            );

        if self.adding {
            page = page
                .child(gpui::div().flex().flex_col().gap_2().w_full().child({
                    #[allow(clippy::expect_used)] // created in this same render
                    let state = self.uri_state.clone().expect("uri field created");
                    LabeledField::new(
                        "totp-uri-input",
                        "otpauth:// URI",
                        "otpauth://totp/… (HOTP not supported)",
                        &state,
                    )
                    .feedback(self.save_error.clone().map(|e| Feedback {
                        kind: FeedbackKind::Error,
                        message: e.into(),
                    }))
                    .disabled(false)
                }))
                .child(
                    Button::new(
                        "totp-save-button",
                        "Save token",
                        ButtonVariant::Primary,
                        move |_, _, cx| {
                            handle.update(cx, Self::add_token);
                        },
                    )
                    .full_width(true),
                );
        } else {
            page = page.child(
                Button::new(
                    "totp-add-button",
                    "Add token",
                    ButtonVariant::Ghost,
                    move |_, _, cx| {
                        handle.update(cx, |this, cx| {
                            this.adding = true;
                            cx.notify();
                        });
                    },
                )
                .full_width(true),
            );
        }
        page
    }
}
