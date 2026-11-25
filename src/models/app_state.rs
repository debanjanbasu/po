//! Persisted UI state (nav bar order, last active route, theme) serialised
//! locally via dual-format `CrdtModel` (Cap'n Proto + automerge) and synced
//! wholesale across devices as identity DHT subkey 2.

use crate::{
    helpers::capnp::{capnp_u32, pack, unpack},
    models::{generated::app_state_capnp, pending_operation::OpType},
    router::nav_item::{NavItemData, NavItemId},
    storage::{snapshots::RetentionPolicy, store_trait::impl_crdt_model},
    user::identity_layout::SUBKEY_APP_STATE,
};

use anyhow::Result;
use autosurgeon::{Hydrate, Reconcile};
use std::{collections::HashSet, str::FromStr};

/// Current schema version for the `AppState` Cap'n Proto struct.
const APP_STATE_SCHEMA_VERSION: u32 = 1;

/// Application-level UI state persisted per user.
///
/// Fields are CRDT-friendly primitives. Nav items are stored as string IDs
/// and reconciled with canonical `NavItemData` at the call site.
#[derive(Clone, Debug, Default, PartialEq, Eq, Reconcile, Hydrate)]
pub struct AppState {
    pub nav_order: Vec<String>,
    /// Automerge docs written before this field existed lack the key; the
    /// `missing` fallback lets older docs hydrate (CRDT-side schema
    /// migration) instead of failing the whole subkey-2 pull.
    #[autosurgeon(missing = "String::default")]
    pub last_active_route: String,
    pub theme: String,
    pub retention_policy: RetentionPolicy,
}

impl AppState {
    /// Reconcile stored nav order with canonical items.
    ///
    /// Preserves the user's saved order while ensuring newly added canonical
    /// items are appended and duplicates are removed.
    #[must_use]
    pub fn reconcile_nav(&self, canonical_items: &[NavItemData]) -> Vec<NavItemData> {
        let mut seen = HashSet::new();
        let mut reconciled: Vec<NavItemData> = self
            .nav_order
            .iter()
            .filter_map(|label| NavItemId::from_str(label).ok())
            .filter_map(|id| canonical_items.iter().find(|c| c.id == id).copied())
            .filter(|item| seen.insert(item.id))
            .collect();
        for &canonical in canonical_items {
            if !seen.contains(&canonical.id) {
                if let Some(acc_pos) = reconciled.iter().position(|i| i.id == NavItemId::Account) {
                    reconciled.insert(acc_pos, canonical);
                } else {
                    reconciled.push(canonical);
                }
            }
        }
        reconciled
    }
}

fn pack_app_state(state: &AppState) -> Result<Vec<u8>> {
    pack::<app_state_capnp::AppState>(|mut root| {
        root.schema_version().set(APP_STATE_SCHEMA_VERSION);
        let len = capnp_u32(state.nav_order.len());
        let mut labels_builder = root.nav_bar_labels().init(len);
        for (i, label) in state.nav_order.iter().enumerate() {
            let idx = capnp_u32(i);
            labels_builder.at(idx).set_str(label);
        }
        root.last_active_route().set_str(&state.last_active_route);
        root.theme().set_str(&state.theme);
        root.retention_policy().set(state.retention_policy.to_u8());
    })
}

fn unpack_app_state(bytes: &[u8]) -> Result<(AppState, u32)> {
    unpack::<app_state_capnp::AppState, _>(bytes, |root| {
        let labels = root.nav_bar_labels().get();
        let nav_order: Vec<String> = (0..labels.len())
            .map(|i| Ok(labels.at(i).get().as_str()?.to_owned()))
            .collect::<Result<_>>()?;
        let last_active_route = root.last_active_route().get().as_str()?.to_owned();
        let theme = match root.theme().get().as_str() {
            Ok(s) if !s.is_empty() => s.to_owned(),
            _ => "system".to_owned(),
        };
        Ok((
            AppState {
                nav_order,
                last_active_route,
                theme,
                retention_policy: RetentionPolicy::from_u8(root.retention_policy()),
            },
            root.schema_version(),
        ))
    })
}

impl_crdt_model!(
    AppState,
    id = "app_state",
    display = "AppState",
    op = OpType::AppState,
    schema = APP_STATE_SCHEMA_VERSION,
    pack = pack_app_state,
    unpack = unpack_app_state,
    subkey = SUBKEY_APP_STATE,
);
