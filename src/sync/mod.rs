//! Shared sync infrastructure: drain loop, pending operations, network-change notify.
//! See also: docs/sync-pipeline.md.

use tokio::sync::Notify;

pub mod handler;
pub mod mutate;
pub mod pending_ops;
pub mod queue;
mod queue_policy;

/// Notification signal for network state changes.
/// Fired by `handle_attachment` in `veilid/core/updates.rs`.
pub static NETWORK_CHANGED: Notify = Notify::const_new();

/// Notification signal for app activity/lifecycle nudges.
pub static APP_ACTIVITY_CHANGED: Notify = Notify::const_new();
