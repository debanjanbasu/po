//! `RootView` — the gpui app shell (the port of `src/app/router.rs`'s
//! `ActiveRouter` + `src/router/nav_layout.rs`'s `NavLayout` dock).
//!
//! Auth-phase machine (verbatim derivation from the old memo):
//! `Restoring` → splash, `PendingBiometric` → biometric gate,
//! `PendingSecondFactor` → PIN gate, `Authenticated` + unhydrated → splash,
//! `Authenticated` + needs onboarding → `PostRegister`, otherwise the
//! authenticated dock over the active route. `Anonymous` → the pre-auth
//! router.

use gpui::{App, Context, Entity, Render, SharedString, Window, prelude::*, px};

use crate::{
    session::phase::SessionPhase,
    ui::{
        router::{AuthRoute, active, active_auth_route},
        state::{app_state_is_hydrated, session_phase},
        widgets::page::CenteredPage,
    },
};

/// Which screen the phase machine selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthPhase {
    Splash,
    BiometricGate,
    PinGate,
    Onboarding,
    Authenticated,
    Guest,
}

fn auth_phase(cx: &App) -> AuthPhase {
    match session_phase(cx) {
        SessionPhase::Restoring => AuthPhase::Splash,
        SessionPhase::PendingBiometric(_) => AuthPhase::BiometricGate,
        SessionPhase::PendingSecondFactor(_) => AuthPhase::PinGate,
        SessionPhase::Authenticated(_) if !app_state_is_hydrated() => AuthPhase::Splash,
        SessionPhase::Authenticated(user) if user.needs_onboarding() => AuthPhase::Onboarding,
        SessionPhase::Authenticated(_) => AuthPhase::Authenticated,
        SessionPhase::Anonymous => AuthPhase::Guest,
    }
}

/// The one root view; the boot entry opens exactly one window over it.
/// Screen entities cache per phase — created on first mount, kept so their
/// window-bound field states persist across re-renders.
#[derive(Default)]
pub struct RootView {
    login: Option<Entity<crate::ui::auth::login_signup::LoginSignup>>,
    onboarding: Option<Entity<crate::ui::auth::post_register::PostRegister>>,
    chat: Option<Entity<crate::ui::chat::chat_list::ChatListView>>,
    totp: Option<Entity<crate::ui::screens::totp::TotpView>>,
    pin: Option<Entity<crate::ui::auth::pin_gate::PinGate>>,
}

impl RootView {
    pub const fn new() -> Self {
        Self {
            login: None,
            onboarding: None,
            chat: None,
            totp: None,
            pin: None,
        }
    }

    /// Progressive-disclosure helper: a step renders only while open.
    fn step(open: bool, body: impl gpui::IntoElement) -> gpui::AnyElement {
        if open {
            body.into_any_element()
        } else {
            gpui::div().into_any_element()
        }
    }

    /// The authenticated dock + active screen (the `NavLayout` port: side
    /// rail on wide windows, bottom dock on narrow ones). Per-route screen
    /// entities cache on `self`.
    fn dock(
        &mut self,
        route: &crate::ui::router::Route,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = crate::ui::theme::Theme::for_appearance(cx);
        let nav_items = [
            ("Chat", crate::ui::router::Route::Chat),
            ("TOTP", crate::ui::router::Route::Totp),
            ("Account", crate::ui::router::Route::Account),
            ("AI", crate::ui::router::Route::Ai),
            ("Calendar", crate::ui::router::Route::Calendar),
            ("Calls", crate::ui::router::Route::Calls),
        ];
        let route_entity_handle = cx
            .try_global::<crate::ui::router::RouteStateGlobal>()
            .map(|g| g.entity.clone());

        // The active screen: the real `ChatList` for Chat routes, `TotpView`
        // for TOTP, placeholders for the not-yet-ported screens.
        let screen: gpui::AnyElement = match route {
            crate::ui::router::Route::Chat => self
                .chat
                .get_or_insert_with(|| cx.new(crate::ui::chat::chat_list::ChatListView::new))
                .clone()
                .into_any_element(),
            crate::ui::router::Route::Totp => self
                .totp
                .get_or_insert_with(|| cx.new(crate::ui::screens::totp::TotpView::new))
                .clone()
                .into_any_element(),
            crate::ui::router::Route::Ai => crate::ui::screens::placeholders::Ai.into_any_element(),
            crate::ui::router::Route::Calendar => {
                crate::ui::screens::placeholders::Calendar.into_any_element()
            }
            crate::ui::router::Route::Calls => {
                crate::ui::screens::placeholders::Calls.into_any_element()
            }
            other => {
                let title = match other {
                    crate::ui::router::Route::Account
                    | crate::ui::router::Route::Profile
                    | crate::ui::router::Route::Security
                    | crate::ui::router::Route::Snapshots
                    | crate::ui::router::Route::AccountPairing => "Account",
                    crate::ui::router::Route::PageNotFound { .. } => "Not found",
                    _ => "po",
                };
                placeholder_screen(title, cx).into_any_element()
            }
        };

        gpui::div()
            .relative()
            .size_full()
            .bg(theme.base_100)
            .flex()
            .child(
                // Side rail (desktop widths).
                gpui::div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .border_r_1()
                    .border_color(theme.base_300)
                    .children(nav_items.map(|(label, target)| {
                        let selected = target == *route;
                        gpui::div()
                            .id(SharedString::from(format!("dock-{label}")))
                            .cursor_pointer()
                            .px_3()
                            .py_2()
                            .text_sm()
                            .rounded(crate::ui::theme::shape::RADIUS_FIELD)
                            .when(selected, |item| {
                                item.bg(theme.primary.fill)
                                    .text_color(theme.primary.content)
                            })
                            .when(!selected, |item| {
                                item.text_color(theme.base_content)
                                    .hover_style(|s| s.bg(theme.base_200))
                            })
                            .on_click({
                                let entity = route_entity_handle.clone();
                                move |_, _, cx| {
                                    if let Some(entity) = entity.as_ref() {
                                        let entity = entity.clone();
                                        entity.update(cx, |state, _| {
                                            state.active = target.clone();
                                        });
                                    }
                                }
                            })
                            .child(label)
                    })),
            )
            .child(
                gpui::div()
                    .id("screen-host")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(screen),
            )
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Re-render on session-phase and route changes (the port of the
        // dioxus effect that re-derived the phase memo). Deep links win over
        // whatever route was active (the old `on_update` hook).
        crate::ui::router::apply_pending_deep_link(cx);
        let phase = auth_phase(cx);
        match phase {
            AuthPhase::Splash => splash(cx).into_any_element(),
            AuthPhase::BiometricGate => {
                placeholder_screen("Biometric check", cx).into_any_element()
            }
            AuthPhase::PinGate => {
                let pin = self
                    .pin
                    .get_or_insert_with(|| cx.new(crate::ui::auth::pin_gate::PinGate::new));
                pin.clone().into_any_element()
            }
            AuthPhase::Onboarding => {
                let onboarding = self.onboarding.get_or_insert_with(|| {
                    cx.new(crate::ui::auth::post_register::PostRegister::new)
                });
                onboarding.clone().into_any_element()
            }
            AuthPhase::Guest => match active_auth_route(cx) {
                AuthRoute::LoginSignup => {
                    let login = self.login.get_or_insert_with(|| {
                        cx.new(crate::ui::auth::login_signup::LoginSignup::new)
                    });
                    login.clone().into_any_element()
                }
                AuthRoute::Pairing => placeholder_screen("Pair a device", cx).into_any_element(),
            },
            AuthPhase::Authenticated => self.dock(&active(cx), cx).into_any_element(),
        }
    }
}

fn splash(cx: &App) -> impl IntoElement {
    let theme = crate::ui::theme::Theme::for_appearance(cx);
    CenteredPage::new().child(
        gpui::div()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .child(
                gpui::svg()
                    .data(crate::ui::assets::logo())
                    .size(px(64.0))
                    .text_color(theme.base_content),
            )
            .child(
                gpui::div()
                    .text_sm()
                    .text_color(theme.text_dim())
                    .child("Restoring…"),
            ),
    )
}

fn placeholder_screen(title: &str, cx: &App) -> impl IntoElement {
    let theme = crate::ui::theme::Theme::for_appearance(cx);
    CenteredPage::new().contained(true).child(
        gpui::div()
            .text_lg()
            .text_color(theme.base_content)
            .child(title.to_string()),
    )
}
