//! Route handling helpers: URL reading, persistence, and `RouterConfig` creation.

use dioxus::prelude::*;
use dioxus_router::RouterConfig;

use crate::{
    global_state::{last_active_route, nav_bar_items, theme},
    helpers::spawn,
    models::app_state::AppState,
    router::Route,
    session::epoch::SessionGuard,
    storage::background_errors::report_nonfatal,
    sync::mutate::{MutationResult, mutate},
    user::ctx::UserCtx,
};

#[derive(Clone, Copy)]
struct RouteRestoration {
    guard: SessionGuard,
    restored: bool,
}

/// Hydration and route restoration belong to an admission, not the process.
/// A previous account's open gate must never authorize persisting defaults.
static ROUTE_RESTORED: GlobalSignal<Option<RouteRestoration>> = Signal::global(|| None);

pub fn mark_app_state_hydrated(guard: SessionGuard, saved_route: &str) {
    guard.commit(|| {
        if ROUTE_RESTORED
            .peek()
            .is_some_and(|state| state.guard == guard)
        {
            return;
        }
        saved_route.clone_into(&mut last_active_route().write());
        *ROUTE_RESTORED.write() = Some(RouteRestoration {
            guard,
            restored: false,
        });
    });
}

pub fn app_state_is_hydrated() -> bool {
    ROUTE_RESTORED
        .read()
        .is_some_and(|state| state.guard.is_current())
}

pub fn route_is_restored() -> bool {
    ROUTE_RESTORED
        .read()
        .is_some_and(|state| state.guard.is_current() && state.restored)
}

/// Called by the `NavLayout` restore effect after it has consumed the stored
/// route (whether or not it actually navigated). Unblocks persistence.
pub fn mark_route_restored() {
    let state = *ROUTE_RESTORED.peek();
    if let Some(state) = state
        && state.guard.is_current()
        && !state.restored
    {
        *ROUTE_RESTORED.write() = Some(RouteRestoration {
            restored: true,
            ..state
        });
    }
}

/// Reactive effect hook that persists `LAST_ACTIVE_ROUTE`, `NAV_BAR_ITEMS`,
/// and `THEME` to local storage whenever any of them changes.
///
/// Must be called from a stable component scope (e.g. `NavLayout`) so the
/// spawned async task has a valid runtime context for polling.
///
/// NOTE: Signal reads happen before the restore gate so that Dioxus always
/// registers reactive subscriptions regardless of gate state.
pub fn use_persist_app_state() {
    use_effect(move || {
        // Read signals FIRST to always register reactive dependencies,
        // even when the gate blocks the persist (otherwise the effect
        // would have zero subscriptions and never wake up).
        let last_active_route = last_active_route().read().clone();
        let nav_order: Vec<String> = nav_bar_items()
            .read()
            .iter()
            .map(|i| i.id.as_str().to_string())
            .collect();
        let theme = theme().read().clone();

        // Reactive read: when restore completes, the effect re-runs and
        // persists anything changed during the gated window.
        if !route_is_restored() {
            return;
        }

        let Some(ctx) = UserCtx::try_current() else {
            return;
        };

        // The route field only persists real destinations: "/" is the
        // transient pre-login route and would break restore-on-launch.
        // Nav order and theme are not route-dependent — persisting them
        // must never be hostage to which screen the user happens to be on.
        let persist_route = !last_active_route.is_empty() && last_active_route != "/";

        spawn(async move {
            if let Err(e) = mutate::<AppState, ()>(&ctx, |state| {
                let route_changed = persist_route && state.last_active_route != last_active_route;
                let changed = state.nav_order != nav_order || route_changed || state.theme != theme;
                if !changed {
                    return Ok(MutationResult::unchanged(()));
                }
                state.nav_order = nav_order;
                if persist_route {
                    state.last_active_route = last_active_route;
                }
                state.theme = theme;
                Ok(MutationResult::modified(()))
            })
            .await
            {
                report_nonfatal("router.helpers", "Failed to persist AppState", e);
            }
        });
    });
}

/// Returns a [`RouterConfig`] whose `on_update` callback writes the
/// active route to `LAST_ACTIVE_ROUTE`. Actual storage persistence is
/// handled reactively by [`use_persist_app_state`] in `NavLayout`.
///
/// Created once via `thread_local!` to satisfy `Callback`'s `Clone` bound.
pub fn route_update_config(_: ()) -> RouterConfig<Route> {
    RouterConfig::default().on_update(|ctx| {
        let route: Route = ctx.current();
        let path = route.to_string();
        // peek: a router callback, not a reactive scope.
        if path == "/"
            || !ROUTE_RESTORED
                .peek()
                .is_some_and(|state| state.guard.is_current() && state.restored)
        {
            return None;
        }
        if last_active_route().peek().as_str() != path.as_str() {
            last_active_route().write().clone_from(&path);
        }
        None
    })
}
