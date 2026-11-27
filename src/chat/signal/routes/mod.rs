//! Peer-route lifecycle: local route creation/publication (in-flight guard +
//! retry, heartbeat refresh) and dead-route cleanup for local and remote routes.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{Context, Error, Result};
use dioxus::{
    logger::tracing::{debug, info, warn},
    prelude::ReadableExt,
};
use tokio::sync::RwLock;
use veilid_core::{KeyPair, RecordKey, RouteBlob, RouteId};

use crate::{
    config,
    global_state::network_status,
    helpers::async_utils::{sleep_ms, timeout},
    platform::device_id::device_id,
    session::epoch::{SessionGuard, spawn_session_scoped},
    storage::background_errors::{push_background_error, report_nonfatal},
    user::{ctx::UserCtx, suspect_push_path},
    veilid::core::{get_veilid_api, parse_record_key},
};

mod routes_dht;
mod routes_remote;
mod routes_send;

pub use routes_remote::{clear_route_cache, evict_routes_for_identity};
pub use routes_send::{prepare_signal_bytes, send_signal_as};
pub(super) use routes_send::{prepare_signal_bytes_as, send_prepared_signal};

struct LocalRoute {
    route_id: RouteId,
    /// The published route blob, kept so the freshness heartbeat can
    /// re-stamp the DHT entry without allocating a new route.
    blob: Vec<u8>,
}

static LOCAL_ROUTE: RwLock<Option<LocalRoute>> = RwLock::const_new(None);

/// Creates a new private route and caches it. Called on attach/reconnect.
/// Returns the route blob for publishing to DHT.
async fn create_local_route() -> Result<Vec<u8>> {
    let api = get_veilid_api().await?;
    // Bounded: on hostile networks route compilation/testing can hang
    // indefinitely, which would wedge PUBLISH_IN_FLIGHT and silently
    // starve the heartbeat (subkey 8 never gets written).
    let RouteBlob { route_id, blob } = timeout(
        Duration::from_millis(config::route::CREATE_TIMEOUT_MS),
        api.new_private_route(),
    )
    .await
    .context("private route allocation timed out")??;

    // Do not hold LOCAL_ROUTE while Veilid compiles/tests the replacement.
    let old = LOCAL_ROUTE.write().await.replace(LocalRoute {
        route_id,
        blob: blob.clone(),
    });
    if let Some(old) = old
        && let Err(e) = api.release_private_route(old.route_id)
    {
        report_nonfatal(
            "chat.signal.routes",
            "Failed to release superseded private route (may leak until expiry)",
            e,
        );
    }
    Ok(blob)
}

/// Releases the current private route (on shutdown/logout).
pub async fn release_local_route() {
    let old = LOCAL_ROUTE.write().await.take();
    if let Some(route) = old
        && let Ok(api) = get_veilid_api().await
        && let Err(e) = api.release_private_route(route.route_id)
    {
        report_nonfatal(
            "chat.signal.routes",
            "Failed to release private route on shutdown (may leak until expiry)",
            e,
        );
    }
}

/// Handle dead local and remote routes reported by Veilid.
///
/// * Dead local routes: if our `LOCAL_ROUTE` died, republish a fresh one.
/// * Dead remote routes: evict matching cached device routes.
pub async fn handle_dead_routes(dead: &[RouteId], dead_remote: &[RouteId]) {
    if !dead.is_empty() {
        let local_died = {
            let guard = LOCAL_ROUTE.read().await;
            guard.as_ref().is_some_and(|lr| dead.contains(&lr.route_id))
        };
        if local_died {
            info!("Local private route died, republishing");
            // Sibling-device nudges are delivered over this route, so losing it
            // is evidence a nudged change may have been missed — have the
            // inspect poll come back fast rather than at its slow ceiling.
            suspect_push_path();
            publish_local_route();
        }
    }

    routes_remote::handle_dead_remote_routes(dead_remote).await;
}

/// Guards against overlapping publish attempts; reset by [`InFlightGuard`].
static PUBLISH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Resets [`PUBLISH_IN_FLIGHT`] even if the retry task is canceled mid-await.
struct InFlightGuard;

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        PUBLISH_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Which stage of a publish attempt failed, for stage-specific reporting.
enum PublishFailure {
    CreateRoute(Error),
    /// The session has no identity record yet — the publish has nowhere to
    /// write and must be retried once onboarding creates it.
    NoIdentity,
    WriteDht(Error),
}

/// Creates a private route and publishes it to the user's identity DHT record.
///
/// Fire-and-forget: the work runs in a session-scoped task that retries with
/// backoff, because right after startup or reconnect the DHT usually cannot
/// accept writes yet. Errors are surfaced only after the final attempt.
/// Concurrent calls coalesce into the already-running attempt, which creates
/// a fresh route on every try.
pub fn publish_local_route() {
    if PUBLISH_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    // Capture the guard before handing the future to the session task, so
    // dropping an unpolled future releases the in-flight flag.
    let in_flight = InFlightGuard;
    spawn_session_scoped(async move {
        let _in_flight = in_flight;
        publish_with_retry().await;
    });
}

async fn publish_with_retry() {
    let session = SessionGuard::capture();
    let max_attempts = config::route::PUBLISH_MAX_ATTEMPTS;

    for attempt in 1..=max_attempts {
        if !session.is_current() {
            return;
        }
        let failure = match try_publish_once().await {
            Ok(()) => return,
            Err(failure) => failure,
        };
        if attempt < max_attempts {
            // attempt is 1-based; the retry after attempt N waits base*2^(N-1).
            let delay_ms = crate::helpers::async_utils::exponential_backoff_ms(
                attempt.saturating_sub(1),
                config::route::PUBLISH_RETRY_BASE_MS,
                None,
            );
            match &failure {
                PublishFailure::CreateRoute(e) | PublishFailure::WriteDht(e) => debug!(
                    "Route publish attempt {attempt}/{max_attempts} failed; retrying in {delay_ms}ms: {e:#}"
                ),
                PublishFailure::NoIdentity => debug!(
                    "Route publish attempt {attempt}/{max_attempts} found no identity yet; retrying in {delay_ms}ms"
                ),
            }
            sleep_ms(delay_ms).await;
            continue;
        }
        match failure {
            PublishFailure::CreateRoute(e) => {
                report_nonfatal("chat.signal", "Failed to create local private route", e);
                push_background_error(
                    "chat.signal",
                    "Could not create peer route. Realtime signals may be unavailable.",
                );
                // No route was ever published, so siblings have nothing to nudge
                // this device over: the fast inspect cadence is the only push
                // path left while creation keeps failing.
                suspect_push_path();
            }
            PublishFailure::NoIdentity => {
                // No identity after the full retry budget: the session never
                // completed onboarding, so there was never a route to
                // publish. The heartbeat retries once the identity exists.
                // Not push-path evidence — there is no record to inspect yet
                // either, so the poll would find no identity and return.
                debug!("Route publish gave up: no identity record for this session");
            }
            PublishFailure::WriteDht(e) => {
                report_nonfatal("chat.signal", "Failed to publish route to DHT", e);
                push_background_error(
                    "chat.signal",
                    "Could not publish peer route. Realtime signals may be delayed.",
                );
                // Whatever the reason (repeated supersede, or a DHT that will
                // not take the write), peers cannot reach this device now, so
                // assume a change may have been missed.
                suspect_push_path();
            }
        }
    }
}

/// One attempt: fresh private route + read-modify-write publish to the
/// identity record.
///
/// "No identity yet" is a **retryable** outcome, not success: at session
/// start the identity is created by an async `CreateIdentity` op, so a
/// publish that runs before it lands has nowhere to write. Treating that as
/// success ended the retry loop, and the device stayed unreachable until
/// the route heartbeat happened to publish (up to 45 s later) — long enough
/// for a peer's first invite to find an empty route subkey.
async fn try_publish_once() -> Result<(), PublishFailure> {
    let blob = create_local_route()
        .await
        .map_err(PublishFailure::CreateRoute)?;

    let Some((dht_key, keypair)) = current_identity_for_publish() else {
        return Err(PublishFailure::NoIdentity);
    };

    // No logging here: `publish_with_retry` reports every failure variant with
    // the attempt number, the delay and the error chain, which is strictly more
    // than a per-attempt line at this depth would say.
    routes_dht::publish_route_to_dht(dht_key, keypair, &blob)
        .await
        .map_err(PublishFailure::WriteDht)
}

/// The authenticated identity record key + writer keypair, or `None` when
/// no session is active (nothing to publish routes for).
fn current_identity_for_publish() -> Option<(RecordKey, Option<KeyPair>)> {
    let ctx = UserCtx::try_current()?;
    let key = parse_record_key(&ctx.dht_key).ok()?;
    Some((key, ctx.keypair))
}

/// This device's stable share of the heartbeat spread, in milliseconds.
///
/// Every device on the account re-stamps the *same* identity subkey with a
/// read-merge-write, and a write that lands on a sibling's concurrent republish
/// is rejected as superseded (`publish_route_to_dht`), spending a publish
/// attempt and letting this device's entry age if it keeps losing. A phase
/// folded from this device's own id — the id that keys its entry in that list —
/// is stable across restarts, so account devices only collide when their shares
/// land within one DHT round trip of each other.
///
/// Bounded by a quarter of the beat period, so a beat plus its share, doubled
/// for one missed beat, still lands inside [`config::route::FRESHNESS_MS`]
/// (`2 × (45s + 11s) < 120s`) — the margin [`config::route::HEARTBEAT_MS`] is
/// chosen to keep.
fn beat_spread_ms(device_id: &str) -> u64 {
    let share = device_id
        .bytes()
        .fold(0_u64, |acc, byte| acc.wrapping_mul(31) + u64::from(byte));
    share % (config::route::HEARTBEAT_MS / 4).max(1)
}

/// Session-scoped freshness heartbeat for this device's published route.
///
/// Route entries in identity subkey 8 are only trusted for
/// [`config::route::FRESHNESS_MS`] — without a periodic re-stamp, a
/// device's entry goes stale ~2 minutes after its last event-driven
/// publish (admission, attach edge, route death) and sibling nudges
/// silently degrade to `delivered=false` even while it is alive and
/// reachable. This loop re-publishes the *existing* route blob with a
/// fresh timestamp; it never allocates a new route (route churn would
/// invalidate what siblings have imported and cached). Each beat is offset by
/// [`beat_spread_ms`] so an account's devices do not all write that one subkey
/// at the same instant.
pub fn start_route_heartbeat() {
    spawn_session_scoped(async {
        let session = SessionGuard::capture();
        let beat_ms = match device_id().await {
            Ok(id) => config::route::HEARTBEAT_MS + beat_spread_ms(&id),
            Err(error) => {
                warn!("Route heartbeat running unspread: cannot read device id: {error}");
                config::route::HEARTBEAT_MS
            }
        };
        loop {
            sleep_ms(beat_ms).await;
            if !session.is_current() {
                return;
            }
            // Transport gate on purpose, unlike the record ops elsewhere: a
            // beat that skips costs entry freshness, while one that runs and
            // fails costs only a quiet debug line (below) and is retried next
            // beat. Keeping the looser gate re-stamps through a readiness flap.
            if !network_status().peek().has_connectivity() {
                info!("Route heartbeat skipped: no connectivity");
                continue;
            }
            // A full publish is already re-stamping the entry.
            if PUBLISH_IN_FLIGHT.load(Ordering::Acquire) {
                info!("Route heartbeat skipped: route publish in flight");
                continue;
            }
            let blob = LOCAL_ROUTE
                .read()
                .await
                .as_ref()
                .map(|route| route.blob.clone());
            let Some(blob) = blob else {
                // No live route (creation failed earlier) — retry the full
                // publish path instead of heartbeating nothing.
                publish_local_route();
                continue;
            };
            let Some((dht_key, keypair)) = current_identity_for_publish() else {
                continue;
            };
            match routes_dht::publish_route_to_dht(dht_key, keypair, &blob).await {
                Ok(()) => {
                    debug!("Route heartbeat re-stamped this device's entry");
                }
                Err(e) => {
                    debug!("Route heartbeat re-stamp failed; retrying next beat: {e:#}");
                }
            }
        }
    });
}
