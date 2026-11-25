//! Consolidated global application state signals.
//!
//! Groups related `GlobalSignal`s into a single access point to reduce
//! scattered statics and improve discoverability.

use std::{collections::HashMap, sync::Arc};

use crate::{
    chat::conversation::ConversationMetaSnapshot, router::nav_item::NavItemData,
    veilid::network::NetworkStatus,
};

use dioxus::prelude::*;

static NAV_BAR_ITEMS: GlobalSignal<Vec<NavItemData>> = Signal::global(Vec::new);

static LAST_ACTIVE_ROUTE: GlobalSignal<String> = Signal::global(String::new);

static THEME: GlobalSignal<String> = Signal::global(|| "system".to_owned());

static NETWORK_STATUS: GlobalSignal<NetworkStatus> = Signal::global(NetworkStatus::default);

static CONVERSATION_META: GlobalSignal<HashMap<Arc<str>, ConversationMetaSnapshot>> =
    Signal::global(HashMap::new);

/// User-reordered navigation bar items.
pub fn nav_bar_items() -> &'static GlobalSignal<Vec<NavItemData>> {
    &NAV_BAR_ITEMS
}

/// Route the user was last viewing, restored on next session.
pub fn last_active_route() -> &'static GlobalSignal<String> {
    &LAST_ACTIVE_ROUTE
}

/// UI theme preference: `"system"` (default), `"light"`, or `"dark"`.
pub fn theme() -> &'static GlobalSignal<String> {
    &THEME
}

/// Network connectivity status for offline-first behavior.
pub fn network_status() -> &'static GlobalSignal<NetworkStatus> {
    &NETWORK_STATUS
}

/// Latest conversation metadata snapshot for watched records.
/// Updated by `dht_watcher` when `ValueChange` fires for a watched record.
pub fn conversation_meta() -> &'static GlobalSignal<HashMap<Arc<str>, ConversationMetaSnapshot>> {
    &CONVERSATION_META
}
