//! Tier-3 sync backstop: adaptive DHT inspect polling.
//!
//! The push paths — sibling-device nudges (private-route signals) and DHT
//! watches (`ValueChanged`) — can be silently lost: strict/NATted networks
//! where inbound routes fail, web builds limited to outbound `WebSocket`s,
//! watches killed by device sleep. This loop compares local vs network
//! subkey sequence numbers with `InspectValue` (outbound-only, no value
//! transfer, NAT-proof) and pulls the affected scopes when the network
//! holds newer data.
//!
//! Cadence adapts: [`config::inspect_poll::FAST_MS`] while push paths look
//! unhealthy (a poll just caught a missed change), decaying to
//! [`config::inspect_poll::SLOW_MS`] after
//! [`config::inspect_poll::CLEAN_POLLS_TO_SLOW`] consecutive clean polls.
//!
//! A pending wait is cut short by the foreground edge
//! (`sync::APP_ACTIVITY_CHANGED`, raised by `lifecycle::foreground_catchup`
//! on every platform's focus/visibility/resume edge), so a tick a
//! backgrounded or throttled platform delayed lands as soon as the app is
//! active again instead of after the rest of the interval.
//!
//! "Push paths look unhealthy" has two sources: this poll catching a missed
//! change itself, and a push path reporting its own failure through
//! [`suspect_push_path`] — a dead local private route, or a route publish that
//! exhausted its retries. The second is what makes the fast cadence answer
//! evidence instead of only a loss this poll happened to be first to notice.
//!
//! The interval is a ceiling, not an idle timer: a tick costs one
//! `inspect_record` per watched record (the identity record, plus each tracked
//! child record — see [`child_watch::watched_records`]), each fanning out to
//! that record's nodes, so the steady state pays the ceiling rate whether or
//! not anything is wrong. That is deliberate: on the web and behind-NAT builds this
//! backstops, the pushes never arrive at all, so a tick that finds nothing
//! newer is no evidence that the push paths work and must not slow the poll
//! further.

use std::iter;
use std::sync::atomic::{AtomicBool, Ordering};

use dioxus::{logger::tracing::debug, prelude::ReadableExt};
use futures_util::future::join_all;
use tokio::{join, select, sync::Notify};
use veilid_core::DHTReportScope;

use crate::{
    config,
    global_state::network_status,
    helpers::async_utils::sleep_ms,
    platform::lifecycle,
    session::epoch::{SessionGuard, spawn_session_scoped},
    sync::APP_ACTIVITY_CHANGED,
    user::{child_watch, ctx::UserCtx, identity_layout as layout, watcher},
    veilid::{core::parse_record_key, store},
};

/// Evidence that a push path (a DHT watch, or a sibling-device nudge) may have
/// lost a change. Set by [`suspect_push_path`]; consumed by the poll's next
/// tick, which restarts from the fast cadence.
static PUSH_PATH_SUSPECT: AtomicBool = AtomicBool::new(false);

/// Wakes the poll out of a wait when [`PUSH_PATH_SUSPECT`] is set.
static SUSPICION: Notify = Notify::const_new();

/// Reports that a push path just showed it can lose a change, so the next tick
/// should not wait out the slow ceiling.
///
/// The caller is a path that watches its own push channel: a local private
/// route dying (sibling nudges to this device are delivered over it), or a
/// route publish that exhausted its retries (peers have no fresh route to this
/// device at all). Both mean "assume a change may have been missed", which is
/// the same assumption a caught missed change is — hence the same fast cadence,
/// decaying through the same clean streak once the evidence stops.
pub fn suspect_push_path() {
    PUSH_PATH_SUSPECT.store(true, Ordering::Release);
    SUSPICION.notify_one();
}

/// Starts the session-scoped inspect-poll loop. Call once after admission;
/// the task ends itself on logout (session-epoch advance).
pub fn start() {
    spawn_session_scoped(async {
        let guard = SessionGuard::capture();
        let mut clean_streak: u32 = 0;
        loop {
            let interval = if clean_streak < config::inspect_poll::CLEAN_POLLS_TO_SLOW {
                config::inspect_poll::FAST_MS
            } else {
                config::inspect_poll::SLOW_MS
            };
            // The cadence, cut short by the foreground edge and by reported
            // push-path evidence: a platform that froze or throttled this loop
            // while the app was put away must not spend the rest of the
            // interval idle once the app is back
            // (`lifecycle::foreground_catchup` raises the activity signal on
            // every platform's focus/visibility/resume edge), and a path that
            // just failed must not wait out a slow interval to be checked.
            select! {
                () = sleep_ms(interval) => {}
                () = APP_ACTIVITY_CHANGED.notified() => {}
                () = SUSPICION.notified() => {}
            }
            // Report evidence restarts the fast streak. A foreground edge alone
            // does not: it is raised by focus and visibility changes too, so
            // treating it as evidence would pin a desktop or web build to the
            // fast cadence.
            if PUSH_PATH_SUSPECT.swap(false, Ordering::AcqRel) {
                clean_streak = 0;
            }
            if !guard.is_current() {
                return;
            }
            // Every tick below opens or inspects a record, so it needs the
            // same gate veilid-core applies, not mere connectivity.
            if !network_status().peek().dht_ready() {
                continue;
            }
            // Startup-race backstop: until one Full resync succeeds, spend
            // the tick completing it (pulls children + arms child watches)
            // instead of inspecting. The readiness edge normally does this
            // first, but a resync can still die on its own transient failure,
            // so this poll stays the guaranteed retry path.
            if watcher::initial_resync_pending() {
                // A post-pairing hydration burst performs equivalent pulls;
                // duplicating them wastes the bounded DHT-read budget. The
                // latch stays pending meanwhile, so whichever path sees it
                // clear first completes the Full resync.
                if !watcher::hydration_burst_active() {
                    watcher::resync_identity().await;
                }
                continue;
            }
            match poll_once().await {
                Some(0) => {
                    // Clean poll: verified contact, nothing missed.
                    clean_streak = clean_streak.saturating_add(1);
                    lifecycle::note_sync_activity();
                }
                Some(scope) => {
                    // Push paths missed something — pull it and speed up.
                    debug!(
                        "Inspect poll caught changes the push paths (watch/nudge) \
                         missed (scope={scope:#04x}); pulling"
                    );
                    watcher::handle_sync_nudge(scope).await;
                    clean_streak = 0;
                }
                // Inspect failed or no identity: no verified contact, so
                // leave the staleness clock alone, but decay cadence like
                // a clean poll so persistent errors don't pin fast mode.
                None => clean_streak = clean_streak.saturating_add(1),
            }
        }
    });
}

/// One inspect pass over the identity record and the watched child
/// records. Returns the [`config::nudge`] scope bits where the network
/// holds newer data (0 = fully in sync), or `None` when no identity is
/// active or every inspect failed.
async fn poll_once() -> Option<u8> {
    let ctx = UserCtx::try_current()?;
    let identity_key = parse_record_key(&ctx.dht_key).ok()?;

    let child_records = child_watch::watched_records();
    let (identity_res, child_results) = join!(
        async {
            store::inspect_record(&identity_key, None, DHTReportScope::SyncGet)
                .await
                .map(|report| {
                    let mut bits = 0u8;
                    for subkey in report.newer_online_subkeys().iter() {
                        bits |= scope_for_identity_subkey(subkey);
                    }
                    bits
                })
        },
        join_all(child_records.into_iter().map(|(key, feature)| async move {
            store::inspect_record(&key, None, DHTReportScope::SyncGet)
                .await
                .map(|report| {
                    if report.newer_online_subkeys().is_empty() {
                        0
                    } else {
                        scope_for_feature(feature)
                    }
                })
        })),
    );

    let mut scope: u8 = 0;
    let mut any_ok = false;
    for bits in iter::once(identity_res).chain(child_results).flatten() {
        any_ok = true;
        scope |= bits;
    }

    any_ok.then_some(scope)
}

/// Maps a lagging identity subkey to the nudge scope that pulls it.
/// Keypair (never changes in place), routes (peer routing, not user
/// data), and the pairing beacon (own watch flow) are ignored.
const fn scope_for_identity_subkey(subkey: u32) -> u8 {
    match subkey {
        layout::SUBKEY_PROFILE | layout::SUBKEY_SECURITY | layout::SUBKEY_APP_STATE => {
            config::nudge::SCOPE_CORE
        }
        layout::SUBKEY_POINTERS => config::nudge::SCOPE_FULL,
        layout::SUBKEY_DICT_META | layout::SUBKEY_DICT_BLOB => config::nudge::SCOPE_DICT,
        _ => 0,
    }
}

fn scope_for_feature(feature: &str) -> u8 {
    match feature {
        "totp" => config::nudge::SCOPE_TOTP,
        "contacts" => config::nudge::SCOPE_CONTACTS,
        "conversations" => config::nudge::SCOPE_CONVERSATIONS,
        _ => 0,
    }
}
