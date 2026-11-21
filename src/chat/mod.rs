//! Chat module: conversation lifecycle, contacts, signals, local storage, hooks.

use std::sync::Arc;

mod conv_keys;
mod delivery;
pub mod invite;
pub mod manager;
mod models;
pub mod record;
pub mod session_flow;
pub mod signal;
pub mod storage;
pub mod watcher;

// Convenience re-exports for ergonomic `crate::chat::{conversation, contact, ...}` paths.
pub use delivery::mark_delivery_status;
pub use models::{contact, conversation, feed, manifest};

/// An asynchronously-loaded value tagged with the account it was loaded for.
///
/// `pub`, not `pub(crate)`: this module is private below the crate root, which
/// already limits both to the crate (`clippy::redundant_pub_crate`).
pub trait AccountScoped {
    fn account_id(&self) -> &Arc<str>;
}

/// Returns the snapshot only when it was loaded for `account_id`.
///
/// Loads complete asynchronously and can land after the session switched
/// accounts; displaying another account's data is a cross-account leak, so
/// every account-scoped snapshot goes through this one guard.
pub fn current_snapshot<'a, T: AccountScoped>(
    snapshot: Option<&'a T>,
    account_id: Option<&Arc<str>>,
) -> Option<&'a T> {
    let snapshot = snapshot?;
    (account_id == Some(snapshot.account_id())).then_some(snapshot)
}
