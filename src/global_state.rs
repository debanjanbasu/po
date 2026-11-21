//! Consolidated global application state signals.
//!
//! Groups related `GlobalSignal`s into a single access point to reduce
//! scattered statics and improve discoverability.

use std::{collections::HashMap, sync::Arc};

use crate::{
    chat::conversation::ConversationMetaSnapshot, ui::nav_item::NavItemData,
    veilid::network::NetworkStatus,
};

use crate::helpers::signals::{GlobalSignal, Signal};

static NAV_BAR_ITEMS: std::sync::LazyLock<GlobalSignal<Vec<NavItemData>>> =
    std::sync::LazyLock::new(|| Signal::global(Vec::new));

static LAST_ACTIVE_ROUTE: std::sync::LazyLock<GlobalSignal<String>> =
    std::sync::LazyLock::new(|| GlobalSignal::new(String::new()));

static THEME: std::sync::LazyLock<GlobalSignal<String>> =
    std::sync::LazyLock::new(|| Signal::global(|| "system".to_owned()));

static NETWORK_STATUS: std::sync::LazyLock<GlobalSignal<NetworkStatus>> =
    std::sync::LazyLock::new(|| GlobalSignal::new(NetworkStatus::default()));

static CONVERSATION_META: std::sync::LazyLock<
    GlobalSignal<HashMap<Arc<str>, ConversationMetaSnapshot>>,
> = std::sync::LazyLock::new(|| GlobalSignal::new(HashMap::new()));

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
