//! Identity-related DHT change handling.
//!
//! Dispatches incoming `ValueChange` events: chat watchers for manifest/feed
//! records, child-record pulls, scoped identity resyncs, and the merge paths
//! that fold pulled docs back into local state. Owns the startup-resync
//! latch and the post-pairing hydration-burst guard.

pub(in crate::user) mod burst;
pub(in crate::user) mod child_pull;
mod dispatch;
mod merge;
mod resync;
mod shared;

pub use burst::{
    HydrationBurstGuard, begin_hydration_burst, hydration_burst_active, initial_resync_pending,
    reset_initial_resync,
};
pub use dispatch::handle_dht_change;
pub use resync::{handle_sync_nudge, resync_identity};
pub(in crate::user) use shared::prepare_pull;
