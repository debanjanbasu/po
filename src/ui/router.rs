//! Hand-rolled router — the port of `dioxus_router`'s `Route` enum +
//! `ActiveRouter` auth-phase machine.
//!
//! Preserves every path, redirect, and the saved-route restore contract:
//! - `/` → `/chat`, `/login` → `/chat`, `/pair` → `/pairing`
//! - `/chat/:conversation_key` carries its key through navigation
//! - unknown paths → `PageNotFound`
//! - `po://` deep links pre-empt routing to the target screen
//!
//! Web URL/history sync happens at the two edges gpui gives us:
//! [`RouteState::restore_from_path`] at boot and marker emission on nav
//! (`e2e-seams`) so the harness can assert without DOM.

use gpui::{App, AppContext as _, Entity, Global};

/// Authenticated-area routes (the old `Route` enum).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Route {
    /// `/totp`
    Totp,
    /// `/account`
    Account,
    /// `/account/profile`
    Profile,
    /// `/account/security`
    Security,
    /// `/account/snapshots`
    Snapshots,
    /// `/account/pair`
    AccountPairing,
    /// `/chat` (default)
    #[default]
    Chat,
    /// `/chat/:conversation_key`
    ChatConversation { conversation_key: String },
    /// `/ai`
    Ai,
    /// `/calendar`
    Calendar,
    /// `/calls`
    Calls,
    /// `/:..route` fallback
    PageNotFound { route: Vec<String> },
}

impl Route {
    /// The path this route addresses (after redirect resolution).
    pub fn path(&self) -> String {
        match self {
            Self::Totp => "/totp".into(),
            Self::Account => "/account".into(),
            Self::Profile => "/account/profile".into(),
            Self::Security => "/account/security".into(),
            Self::Snapshots => "/account/snapshots".into(),
            Self::AccountPairing => "/account/pair".into(),
            Self::Chat => "/chat".into(),
            Self::ChatConversation { conversation_key } => {
                format!("/chat/{conversation_key}")
            }
            Self::Ai => "/ai".into(),
            Self::Calendar => "/calendar".into(),
            Self::Calls => "/calls".into(),
            Self::PageNotFound { route } => format!("/{}", route.join("/")),
        }
    }

    /// Path → route, applying the enum's redirects. Unknown paths fall to
    /// `PageNotFound` with the consumed segments, exactly as `#[route(
    /// "/:..route")]` did.
    pub fn from_path(path: &str) -> Self {
        let path = path.split('?').next().unwrap_or(path);
        let trimmed = path.trim_end_matches('/');
        match trimmed {
            // The enum's redirects (`/`, `/login`, `/pair`) all resolve to Chat.
            "" | "/" | "/login" | "/pair" | "/chat" => Self::Chat,
            "/totp" => Self::Totp,
            "/account" => Self::Account,
            "/account/profile" => Self::Profile,
            "/account/security" => Self::Security,
            "/account/snapshots" => Self::Snapshots,
            "/account/pair" => Self::AccountPairing,
            "/ai" => Self::Ai,
            "/calendar" => Self::Calendar,
            "/calls" => Self::Calls,
            _ => {
                if let Some(key) = trimmed.strip_prefix("/chat/") {
                    return Self::ChatConversation {
                        conversation_key: key.to_owned(),
                    };
                }
                let segments = trimmed
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                Self::PageNotFound { route: segments }
            }
        }
    }
}

/// Pre-auth routes (the old `AuthRoute` enum): any stale URL shows login.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AuthRoute {
    /// `/` (default; redirects swallow every other path)
    #[default]
    LoginSignup,
    /// `/pair`
    Pairing,
}

impl AuthRoute {
    pub const fn path(&self) -> &'static str {
        match self {
            Self::LoginSignup => "/",
            Self::Pairing => "/pair",
        }
    }
}

/// The active route + navigation side-state, held as one entity so the
/// shell observes it.
#[derive(Clone, Debug, Default)]
pub struct RouteState {
    pub active: Route,
    /// Route captured at boot from the saved `AppState`, consumed once by
    /// the first authenticated render (old `route_is_restored` latch).
    pub restore: Option<Route>,
}

impl Global for RouteStateGlobal {}

pub struct RouteStateGlobal {
    pub entity: Entity<RouteState>,
}

/// Installs the router state at boot.
pub fn init_router(cx: &mut App) {
    let entity = cx.new(|_| RouteState::default());
    cx.set_global(RouteStateGlobal { entity });
}

/// Navigates (the port of `nav()` + `<Link>` presses). Redirect semantics
/// apply, so `nav(Chat)` from `/` lands on `/chat`.
pub fn nav(cx: &mut gpui::App, to: Route) {
    let resolved = match to {
        // The enum's redirects, applied at nav time.
        Route::Chat if matches!(Route::from_path("/"), Route::Chat) => Route::Chat,
        other => other,
    };
    // Persist the route for the next session's restore. Until the cutover,
    // the value lives in the shared `LAST_ACTIVE_ROUTE` signal (read by the
    // Dioxus persistence effect); Phase 7 moves it into this module's state.
    let mut slot = crate::global_state::last_active_route().write();
    slot.clone_from(&resolved.path());
    drop(slot);
    let entity = cx.global::<RouteStateGlobal>().entity.clone();
    entity.update(cx, |state, _| {
        state.active = resolved;
    });
}

/// Current route.
pub fn active(cx: &App) -> Route {
    cx.global::<RouteStateGlobal>()
        .entity
        .read(cx)
        .active
        .clone()
}

/// Restores the last active route from persisted `AppState` at hydration
/// (old `mark_app_state_hydrated` + `use_persist_app_state` contract).
pub fn restore_from_path(cx: &mut App, saved: &str) {
    let route = Route::from_path(saved);
    let entity = cx.global::<RouteStateGlobal>().entity.clone();
    entity.update(cx, |state, _| {
        state.restore = Some(route.clone());
        state.active = route;
    });
}

/// Applies a queued deep-link action's navigation (the port of
/// `ActiveRouter`'s `on_update` hook that jumped to `/pair` for a pending
/// `po://pair` link). Returns whether a nav happened.
pub fn apply_pending_deep_link(cx: &mut App) -> bool {
    if crate::platform::deep_link::has_pending_pairing() {
        nav(cx, Route::AccountPairing);
        return true;
    }
    false
}

// ── Pre-auth routing (the old AuthRoute) ────────────────────────────────

/// Holds the active pre-auth route (login screen vs pairing).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthRouteState {
    pub active: AuthRoute,
}

pub struct AuthRouteGlobal {
    pub entity: Entity<AuthRouteState>,
}

impl Global for AuthRouteGlobal {}

/// Installs pre-auth routing at boot.
pub fn init_auth_router(cx: &mut App) {
    let entity = cx.new(|_| AuthRouteState::default());
    cx.set_global(AuthRouteGlobal { entity });
}

/// Navigates the pre-auth flow (the port of `nav.replace(AuthRoute::…)`).
pub fn nav_auth_route(cx: &mut App, to: AuthRoute) {
    let entity = cx.global::<AuthRouteGlobal>().entity.clone();
    entity.update(cx, |state, _| state.active = to);
}

pub fn active_auth_route(cx: &App) -> AuthRoute {
    cx.global::<AuthRouteGlobal>()
        .entity
        .read(cx)
        .active
        .clone()
}

pub fn auth_route_entity(cx: &App) -> Entity<AuthRouteState> {
    cx.global::<AuthRouteGlobal>().entity.clone()
}
