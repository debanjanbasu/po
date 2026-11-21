//! Startup-resync latch and post-pairing hydration-burst guard.
//!
//! Two latches gate who owns identity pulls right after admission: the
//! pending-resync latch keeps connectivity-edge/inspect-poll retries firing
//! until one Full resync succeeds, and the burst guard lets the pairing
//! hydration task claim those pulls exclusively while it runs.

use std::sync::atomic::{AtomicBool, Ordering};

/// Latched `true` once a Full-scope resync succeeds for the current
/// session. Starts `true` (no session = nothing pending); reset by
/// `complete_admission` before it spawns the startup sync. While `false`,
/// the first-connectivity edge in `veilid::core::updates::workers` re-runs
/// the startup watch-arm + resync, because admission's one-shot attempt
/// often fires before the network is usable (cold boot) and dies silently.
static INITIAL_RESYNC_OK: AtomicBool = AtomicBool::new(true);

/// Mark the startup resync pending for a freshly admitted session.
pub fn reset_initial_resync() {
    INITIAL_RESYNC_OK.store(false, Ordering::Release);
}

/// True while the post-admission Full resync has not yet succeeded.
pub fn initial_resync_pending() -> bool {
    !INITIAL_RESYNC_OK.load(Ordering::Acquire)
}

/// Called by [`super::resync`] when a Full-scope pull lands: proves the
/// startup sync completed and stops the connectivity-edge retry loop.
pub(super) fn mark_initial_resync_done() {
    INITIAL_RESYNC_OK.store(true, Ordering::Release);
}

/// `true` while a post-pairing hydration burst is running. The startup-race
/// retry paths (inspect poll, connectivity edge) hold their pending-latch
/// Full resyncs while set so they don't duplicate the burst's own pulls.
static HYDRATION_BURST: AtomicBool = AtomicBool::new(false);

/// RAII marker for a hydration burst — clears [`HYDRATION_BURST`] on drop so
/// early returns can't leave retry paths latched off.
pub struct HydrationBurstGuard;

impl Drop for HydrationBurstGuard {
    fn drop(&mut self) {
        HYDRATION_BURST.store(false, Ordering::Release);
    }
}

/// Marks a hydration burst active until the returned guard drops.
pub fn begin_hydration_burst() -> HydrationBurstGuard {
    HYDRATION_BURST.store(true, Ordering::Release);
    HydrationBurstGuard
}

/// True while a post-pairing hydration burst owns the identity pull.
pub fn hydration_burst_active() -> bool {
    HYDRATION_BURST.load(Ordering::Acquire)
}
