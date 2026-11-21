//! gpui state layer: the session-phase source of truth and the per-model
//! revision bus. Ports the Dioxus `GlobalSignal` sites that need observers;
//! signals consumed without reactivity stay as plain statics in their own
//! modules (see docs/migration-gpui-ce.md § state table).

use std::{any::TypeId, collections::HashMap};

use gpui::{App, AppContext as _, Entity, Global};

use crate::session::phase::SessionPhase;

// ── Session phase ────────────────────────────────────────────────────────

/// Holds the [`Entity<SessionPhase>`] so any code with an `App`/`Context`
/// can read it and any view can `cx.observe` it. Writes go through
/// [`set_session_phase`], which is the exact port of the old
/// `SESSION.write()` commit points (rare: login/logout/admit).
pub struct SessionPhaseGlobal {
    pub entity: Entity<SessionPhase>,
}

impl Global for SessionPhaseGlobal {}

/// Installs the session-phase entity at boot.
pub fn init_session(cx: &mut App) {
    let entity = cx.new(|_| SessionPhase::default());
    cx.set_global(SessionPhaseGlobal { entity });
}

/// Read-only access (any `App`-derived context).
pub fn session_phase(cx: &App) -> &SessionPhase {
    cx.global::<SessionPhaseGlobal>().entity.read(cx)
}

/// Mutating access: the port of `*SESSION.write() = …`. Notify is automatic
/// on entity update — observers re-run exactly where the old signal effects
/// did.
pub fn set_session_phase<R>(cx: &mut App, f: impl FnOnce(&mut SessionPhase) -> R) -> R {
    let entity = cx.global::<SessionPhaseGlobal>().entity.clone();
    entity.update(cx, |phase, _cx| f(phase))
}

// ── Model revision bus ───────────────────────────────────────────────────

/// One observable revision counter per `CrdtModel`, keyed by `TypeId`.
///
/// Replaces `impl_crdt_model!`'s per-model `GlobalSignal<u64>` statics
/// (4 models today: `ConversationIndex`, `ContactList`, `TotpTokenList`,
/// `AppState`). "Bump on write, observe on read" with cheap per-model
/// identity: writers call [`notify_model_changed::<T>`]; a view subscribes
/// once via [`observe_model::<T>`] and re-reads its data.
#[derive(Default)]
pub struct RevisionBus {
    counters: HashMap<TypeId, Entity<u64>>,
}

impl Global for RevisionBus {}

/// Bumps the revision counter for model `T`, notifying its observers.
///
/// The port of `store_trait::notify_model_changed` — same call sites.
pub fn notify_model_changed<T: 'static>(cx: &mut App) {
    let Some(entity) = cx
        .try_global::<RevisionBus>()
        .and_then(|bus| bus.counters.get(&TypeId::of::<T>()).cloned())
    else {
        // No observers were installed before the first write; nothing to
        // notify. The next `observe_model::<T>` seeds the entity fresh.
        return;
    };
    entity.update(cx, |revision, _cx| *revision = revision.wrapping_add(1));
}

/// Returns the current revision for model `T` (0 if never bumped).
pub fn model_revision<T: 'static>(cx: &App) -> u64 {
    cx.try_global::<RevisionBus>()
        .and_then(|bus| bus.counters.get(&TypeId::of::<T>()))
        .map_or(0, |entity| *entity.read(cx))
}

/// Seeds the counter for model `T` if missing and returns its entity for
/// `cx.observe`. Called from view constructors (the port of
/// `use_model_revision::<T>()`'s subscribe step).
pub fn revision_entity<T: 'static>(cx: &mut App) -> Entity<u64> {
    if let Some(entity) = cx
        .try_global::<RevisionBus>()
        .and_then(|bus| bus.counters.get(&TypeId::of::<T>()).cloned())
    {
        return entity;
    }
    let entity = cx.new(|_| 0u64);
    cx.global_mut::<RevisionBus>()
        .counters
        .insert(TypeId::of::<T>(), entity.clone());
    entity
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ModelA;
    struct ModelB;

    /// Requires the gpui `test-support` feature; exercises the bus the way
    /// the ported views will: seed → bump → read.
    #[test]
    #[cfg(feature = "gpui-test")]
    fn revisions_are_per_model_and_bump_independently() {
        let mut app = gpui::TestApp::new();
        app.update(|cx| {
            init_session(cx);
            assert_eq!(model_revision::<ModelA>(cx), 0);
            let _a = revision_entity::<ModelA>(cx);
            let _b = revision_entity::<ModelB>(cx);
            notify_model_changed::<ModelA>(cx);
            notify_model_changed::<ModelA>(cx);
            assert_eq!(model_revision::<ModelA>(cx), 2);
            assert_eq!(model_revision::<ModelB>(cx), 0);
        });
    }
}

// ── App-state hydration latch ────────────────────────────────────────────

/// Whether the persisted `AppState` finished hydrating for the current
/// session. The port of `router/helpers.rs::app_state_is_hydrated`: the
/// frozen latch is a dioxus signal; this one is a plain static the startup
/// port (cutover item 1) sets. Under the gpui entry it starts `true` — the
/// startup pipeline has not landed yet, and gating on it would pin the shell
/// to the splash forever.
static APP_STATE_HYDRATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// The hydration gate the shell reads (`Authenticated` + not hydrated →
/// splash). Set `false` from the startup port once it owns hydration.
pub fn app_state_is_hydrated() -> bool {
    APP_STATE_HYDRATED.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn set_app_state_hydrated(hydrated: bool) {
    APP_STATE_HYDRATED.store(hydrated, std::sync::atomic::Ordering::SeqCst);
}

// ── App-state hydration (nav/theme/route) ────────────────────────────────

/// The ui-state port of `hydrate_app_state_signals`: reconciles nav items,
/// theme, and last-active route from the persisted `AppState`. Reads/writes
/// go through the same shared statics the frozen stack uses until the
/// global-state cutover completes.
pub fn hydrate_nav_theme_route(state: &crate::models::app_state::AppState) {
    let new_nav = state.reconcile_nav(&crate::ui::nav_item::NAV_ITEMS);
    let nav = crate::global_state::nav_bar_items();
    if *nav.peek() != new_nav {
        nav.write().clone_from(&new_nav);
    }
    let theme = crate::global_state::theme();
    if !state.theme.is_empty() && theme.peek().as_str() != state.theme {
        theme.write().clone_from(&state.theme);
    }
    // Only hydrate once: a non-empty LAST_ACTIVE_ROUTE is never overwritten.
    let route = crate::global_state::last_active_route();
    if !state.last_active_route.is_empty() && route.peek().is_empty() {
        route.write().clone_from(&state.last_active_route);
    }
}
