use crate::{
    components::{BiometricGate, PinGate, PostRegister, RestoreSplash},
    platform::deep_link,
    router::{
        AuthRoute, Route,
        helpers::{app_state_is_hydrated, route_update_config},
    },
    session::phase::{SESSION, SessionPhase},
};
use dioxus::prelude::*;
use dioxus_router::RouterConfig;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthPhase {
    Splash,
    BiometricGate,
    PinGate,
    Onboarding,
    Authenticated,
    Guest,
}

#[component]
pub fn ActiveRouter() -> Element {
    let auth_phase = use_memo(|| match &*SESSION.read() {
        SessionPhase::Restoring => AuthPhase::Splash,
        SessionPhase::PendingBiometric(_) => AuthPhase::BiometricGate,
        SessionPhase::PendingSecondFactor(_) => AuthPhase::PinGate,
        SessionPhase::Authenticated(_) if !app_state_is_hydrated() => AuthPhase::Splash,
        SessionPhase::Authenticated(user) if user.needs_onboarding() => AuthPhase::Onboarding,
        SessionPhase::Authenticated(_) => AuthPhase::Authenticated,
        SessionPhase::Anonymous => AuthPhase::Guest,
    });
    match auth_phase() {
        AuthPhase::Splash => rsx! { RestoreSplash {} },
        AuthPhase::BiometricGate => rsx! { BiometricGate {} },
        AuthPhase::PinGate => rsx! { PinGate {} },
        AuthPhase::Onboarding => rsx! { PostRegister {} },
        AuthPhase::Authenticated => rsx! {
            Router::<Route> { config: route_update_config }
        },
        AuthPhase::Guest => rsx! {
            Router::<AuthRoute> {
                config: |()| {
                    RouterConfig::default().on_update(|state| {
                        // Auto-route to /pair when a pair deep link is pending.
                        // Mounted auth screens observe the same reactive state
                        // for warm links, without discarding their form state.
                        if deep_link::has_pending_pairing()
                            && state.current() != AuthRoute::Pairing
                        {
                            return NavigationTarget::Internal(AuthRoute::Pairing).into();
                        }
                        None
                    })
                }
            }
        },
    }
}
