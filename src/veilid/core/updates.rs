//! Veilid update callback and nonblocking ingress workers.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use tokio::{
    select,
    sync::{
        mpsc,
        watch::{self, Receiver},
    },
};
use tracing::{debug, info, warn};
use veilid_core::{
    VeilidAppMessage, VeilidRouteChange, VeilidStateAttachment, VeilidStateNetwork, VeilidUpdate,
};

use crate::{
    chat::{
        record::retry_failed_hydrations,
        signal::{handle_app_message, handle_dead_routes, publish_local_route},
    },
    config,
    global_state::network_status,
    helpers::{spawn, store_lock::recover_poison},
    nonfatal,
    sync::{NETWORK_CHANGED, pending_ops::retry_all_dead_letters, queue as sync_queue},
    user::{
        child_watch,
        identity_watch::start,
        watcher::{hydration_burst_active, initial_resync_pending, resync_identity},
    },
    veilid::{change_dispatcher, network::NetworkStatus as AppNetworkStatus},
};

const APP_MESSAGE_BUFFER: usize = config::dispatcher::CHANNEL_CAPACITY * 4;
const ROUTE_CHANGE_BUFFER: usize = config::dispatcher::CHANNEL_CAPACITY;

/// Clearable update ingress — replaced on each Veilid init cycle.
static UPDATE_INGRESS: Mutex<Option<Arc<UpdateIngress>>> = Mutex::new(None);

struct UpdateIngress {
    closed: AtomicBool,
    shutdown_tx: watch::Sender<bool>,
    app_message_tx: mpsc::Sender<VeilidAppMessage>,
    route_change_tx: mpsc::Sender<VeilidRouteChange>,
    attachment_tx: watch::Sender<Option<VeilidStateAttachment>>,
    network_tx: watch::Sender<Option<VeilidStateNetwork>>,
}

impl UpdateIngress {
    fn new() -> Arc<Self> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (app_message_tx, app_message_rx) = mpsc::channel(APP_MESSAGE_BUFFER);
        let (route_change_tx, route_change_rx) = mpsc::channel(ROUTE_CHANGE_BUFFER);
        let (attachment_tx, attachment_rx) = watch::channel(None);
        let (network_tx, network_rx) = watch::channel(None);

        spawn_app_message_worker(app_message_rx, shutdown_rx.clone());
        spawn_route_change_worker(route_change_rx, shutdown_rx.clone());
        spawn_attachment_worker(attachment_rx, shutdown_rx.clone());
        spawn_network_worker(network_rx, shutdown_rx);

        Arc::new(Self {
            closed: AtomicBool::new(false),
            shutdown_tx,
            app_message_tx,
            route_change_tx,
            attachment_tx,
            network_tx,
        })
    }

    fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.shutdown_tx.send(true);
    }

    fn handle_update(&self, update: VeilidUpdate) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }

        match update {
            VeilidUpdate::AppMessage(message) => {
                try_send_or_warn(&self.app_message_tx, *message, "app message");
            }
            VeilidUpdate::Attachment(attachment) => {
                self.attachment_tx.send_replace(Some(*attachment));
            }
            VeilidUpdate::ValueChange(change) => {
                // Tier-1 (watch) ground truth: a line here proves veilid
                // delivered a push notification for a watched record.
                debug!(
                    "DHT ValueChange: key={} subkeys={:?} count={} value={}",
                    change.key,
                    change.subkeys,
                    change.count,
                    change.value.is_some(),
                );
                change_dispatcher::enqueue(*change);
            }
            VeilidUpdate::RouteChange(change) => {
                try_send_or_warn(&self.route_change_tx, *change, "route change");
            }
            VeilidUpdate::Network(network) => {
                self.network_tx.send_replace(Some(*network));
            }
            VeilidUpdate::Shutdown => self.shutdown(),
            VeilidUpdate::Log(_) | VeilidUpdate::AppCall(_) | VeilidUpdate::Config(_) => {}
        }
    }
}

pub(super) fn install_update_ingress() {
    let ingress = UpdateIngress::new();
    let mut guard = recover_poison(UPDATE_INGRESS.lock());
    if let Some(existing) = guard.replace(ingress) {
        existing.shutdown();
    }
}

pub(super) fn clear_update_ingress() {
    let ingress = {
        let mut guard = recover_poison(UPDATE_INGRESS.lock());
        guard.take()
    };
    if let Some(ingress) = ingress {
        ingress.shutdown();
    }
}

pub(super) fn update_callback(update: VeilidUpdate) {
    let ingress = {
        let guard = recover_poison(UPDATE_INGRESS.lock());
        guard.as_ref().cloned()
    };
    if let Some(ingress) = ingress {
        ingress.handle_update(update);
    }
}

fn try_send_or_warn<T>(tx: &mpsc::Sender<T>, item: T, kind: &'static str) {
    match tx.try_send(item) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!("Veilid update ingress saturated for {kind}");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            warn!("Veilid {kind} worker closed before update was handled");
        }
    }
}

// ── Ingress workers ──────────────────────────────────────────────────────────

fn shutdown_requested(
    shutdown_rx: &mut Receiver<bool>,
    changed: &Result<(), watch::error::RecvError>,
) -> bool {
    changed.is_err() || *shutdown_rx.borrow_and_update()
}

fn spawn_mpsc_worker<T, F, Fut>(
    mut receiver: mpsc::Receiver<T>,
    mut shutdown_rx: Receiver<bool>,
    mut on_item: F,
) where
    T: Send + 'static,
    F: FnMut(T) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    spawn(async move {
        loop {
            select! {
                changed = shutdown_rx.changed() => {
                    if shutdown_requested(&mut shutdown_rx, &changed) {
                        break;
                    }
                }
                maybe_item = receiver.recv() => {
                    let Some(item) = maybe_item else {
                        break;
                    };
                    on_item(item).await;
                }
            }
        }
    });
}

fn spawn_watch_worker<T, F, Fut>(
    mut receiver: watch::Receiver<Option<T>>,
    mut shutdown_rx: Receiver<bool>,
    mut on_update: F,
) where
    T: Clone + Send + 'static,
    F: FnMut(Option<T>) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    spawn(async move {
        loop {
            select! {
                changed = shutdown_rx.changed() => {
                    if shutdown_requested(&mut shutdown_rx, &changed) {
                        break;
                    }
                }
                changed = receiver.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let update = receiver.borrow_and_update().clone();
                    on_update(update).await;
                }
            }
        }
    });
}

fn spawn_app_message_worker(
    receiver: mpsc::Receiver<VeilidAppMessage>,
    shutdown_rx: Receiver<bool>,
) {
    spawn_mpsc_worker(receiver, shutdown_rx, |message| async move {
        handle_app_message(message.message());
    });
}

fn spawn_route_change_worker(
    receiver: mpsc::Receiver<VeilidRouteChange>,
    shutdown_rx: Receiver<bool>,
) {
    spawn_mpsc_worker(receiver, shutdown_rx, |change| async move {
        let dead = &change.dead_routes;
        let dead_remote = &change.dead_remote_routes;
        if !dead.is_empty() || !dead_remote.is_empty() {
            handle_dead_routes(dead, dead_remote).await;
        }
    });
}

fn spawn_attachment_worker(
    receiver: watch::Receiver<Option<VeilidStateAttachment>>,
    shutdown_rx: Receiver<bool>,
) {
    spawn_watch_worker(receiver, shutdown_rx, |latest| async move {
        if let Some(attachment) = latest {
            handle_attachment(attachment).await;
        }
    });
}

fn spawn_network_worker(
    receiver: watch::Receiver<Option<VeilidStateNetwork>>,
    shutdown_rx: Receiver<bool>,
) {
    spawn_watch_worker(receiver, shutdown_rx, |latest| async move {
        if let Some(network) = latest {
            handle_network(&network);
        }
    });
}

/// Last network snapshot for dedup (started + peer count).
static LAST_NETWORK_SNAPSHOT: AtomicU64 = AtomicU64::new(u64::MAX);

// ── LAN locality seam ────────────────────────────────────────────────────────
//
// A whole module so a release build pays nothing — not even the address
// classifier or its imports. What it says, and what it cannot say, is recorded
// in docs/device-testing.md § "Local network".

#[cfg(any(debug_assertions, feature = "e2e-seams"))]
mod locality_seam {
    use std::net::{IpAddr, SocketAddr};
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use veilid_core::VeilidStateNetwork;

    use crate::{config, global_state::network_status};

    /// Peer locality of the last `Network` update: `peers << 16 | local_peers`.
    ///
    /// Peer addresses arrive only on `Network` and routing-domain readiness only
    /// on `Attachment`, so one marker is fed from both workers and this remembers
    /// the peer half. [`NONE`] until the first `Network` update has been seen.
    static PEER_LOCALITY: AtomicU32 = AtomicU32::new(NONE);
    const NONE: u32 = u32::MAX;

    /// The last marker emitted, to suppress repeats: bits 0–31 peer locality,
    /// bit 32 `public_internet_ready`, bit 33 `local_network_ready`.
    static LAST_EMITTED: AtomicU64 = AtomicU64::new(u64::MAX);

    /// Records this update's peer locality and emits the marker.
    pub(super) fn note_network(network: &VeilidStateNetwork) {
        let peers = network.peers.len();
        let local_peers = network
            .peers
            .iter()
            .filter(|peer| is_local_address(&peer.peer_address))
            .count();
        PEER_LOCALITY.store(
            ((u32::try_from(peers).unwrap_or(u32::MAX) & 0xFFFF) << 16)
                | (u32::try_from(local_peers).unwrap_or(u32::MAX) & 0xFFFF),
            Ordering::Relaxed,
        );
        emit();
    }

    /// Emits `event=net_locality`, unless nothing it reports has changed.
    #[allow(clippy::significant_drop_tightening)] // the status guard spans three field reads
    pub(super) fn emit() {
        let packed = PEER_LOCALITY.load(Ordering::Relaxed);
        if packed == NONE {
            return; // No peer table yet — nothing honest to report.
        }
        let network = network_status();
        let status = network.peek();
        let public = status.public_internet_ready;
        let local = status.local_network_ready;
        let key = u64::from(packed) | (u64::from(public) << 32) | (u64::from(local) << 33);
        if LAST_EMITTED.swap(key, Ordering::Relaxed) == key {
            return;
        }
        config::e2e::event!(
            "net_locality",
            local_network_ready = local,
            public_internet_ready = public,
            peers = packed >> 16,
            local_peers = packed & 0xFFFF,
        );
    }

    /// Whether a peer's observed address sits on this LAN.
    ///
    /// `peer_address` is veilid's display form, `<PROTO>:<socket addr>` — e.g.
    /// `UDP:192.168.1.42:5150` — and the peer table is the only place the API
    /// exposes any address at all: there is no per-peer routing-domain or
    /// transport field. So locality is *inferred from the address*, and a peer
    /// that reached us through a carrier NAT (100.64/10) is counted as remote
    /// even though veilid-core dials it in its `LocalNetwork` domain.
    fn is_local_address(peer_address: &str) -> bool {
        peer_address
            .split_once(':')
            .and_then(|(_, socket)| socket.parse::<SocketAddr>().ok())
            .is_some_and(|socket| is_local_ip(socket.ip()))
    }

    /// RFC 1918 / link-local / loopback (v4) and ULA / link-local / loopback
    /// (v6) — the addresses that cannot be anywhere but this site.
    const fn is_local_ip(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
            IpAddr::V6(v6) => {
                v6.is_unique_local() || v6.is_unicast_link_local() || v6.is_loopback()
            }
        }
    }
}

fn handle_network(network: &VeilidStateNetwork) {
    #[cfg(any(debug_assertions, feature = "e2e-seams"))]
    locality_seam::note_network(network);

    // Cheap change detection: only log when started or peer count changes.
    let peers = network.peers.len();
    let snapshot = (u64::from(network.started) << 32) | (peers as u64);

    let prev = LAST_NETWORK_SNAPSHOT.swap(snapshot, Ordering::Relaxed);
    if prev == snapshot {
        return; // No meaningful change — skip log entirely.
    }

    if prev == u64::MAX {
        info!(
            "Network state: started={}, bps_down={}, bps_up={}, peers={peers}",
            network.started,
            network.bps_down.as_u64(),
            network.bps_up.as_u64(),
        );
    } else {
        debug!(
            "Network state: started={}, bps_down={}, bps_up={}, peers={peers}",
            network.started,
            network.bps_down.as_u64(),
            network.bps_up.as_u64(),
        );
    }
}

async fn handle_attachment(attachment: VeilidStateAttachment) {
    let was_dht_ready = network_status().peek().dht_ready();
    let now_status = AppNetworkStatus::from_attachment(&attachment);
    let now_dht_ready = now_status.dht_ready();
    // Only write when the status actually changed.
    //
    // `NetworkStatus` carries volatile counters (uptime, median latency,
    // peer counts). Writing it unconditionally made every subscriber
    // re-read the signal on every attachment update — the activity bar's
    // summary memo and the conversation view's connectivity memo — even
    // when the attachment was a verbatim repeat.
    //
    // The comparison is on the whole struct, not on a subset of fields:
    // `has_connectivity()` is derived from `live_peer_count`, and the
    // activity modal renders every counter, so a partial comparison
    // would freeze real state in the UI.
    //
    // This mirrors the repo's own `set_if_changed` convention
    // (`src/storage/mod.rs`).
    let status_changed = *network_status().peek() != now_status;
    if status_changed {
        *network_status().write() = now_status;
    }

    // Wake background tasks and startup retry waits that react to network changes.
    NETWORK_CHANGED.notify_waiters();

    #[cfg(any(debug_assertions, feature = "e2e-seams"))]
    locality_seam::emit();

    // Fire on the DHT edge. Every step below opens or writes a record, so
    // gating on plain transport connectivity let the whole block run while
    // the storage manager was still refusing opens: the work failed, spent
    // retry budget it should not have, and some steps never ran again because
    // no further transport edge followed. `dht_ready` is the signal that
    // storage-manager gate uses, so this edge fires exactly when the work can
    // succeed. Quality-only changes (weak/good/strong) don't re-trigger it.
    //
    // Armed watches need nothing here: veilid-core retains desired watch
    // state across detach, renews and reconciles per-node watches itself,
    // and on routing-domain-ready inspects every watched record for missed
    // changes. Offline subkey writes are flushed by core. Child pointers
    // whose pin failed were never registered with core, so retry them after
    // reconnect; an incomplete startup still uses the full-resync retry.
    if now_dht_ready && !was_dht_ready {
        // App-semantic retries core can't know about.
        nonfatal!(
            "veilid.core.updates",
            "Failed to retry dead-lettered ops on reconnect",
            retry_all_dead_letters().await
        );
        // Retry conversations that failed to hydrate during pairing, off this
        // worker: a multi-minute batch awaited inline would stall the sole
        // `network_status` writer, queueing a detach behind it while
        // `dht_ready` stayed true from the update that started it.
        spawn(retry_failed_hydrations());
        sync_queue::get_sync_queue().notify_enqueue();
        // Publish only on the edge: churn here would invalidate routes
        // peers cached. Route death is handled by `handle_dead_routes`.
        publish_local_route();
        // Startup-race recovery (pull side): admission's one-shot
        // watch-arm + full resync often run before the network is usable
        // at cold boot and die silently — no identity watch all session,
        // child watches never armed. Retry on each readiness edge
        // until one Full resync succeeds (latch), then never again.
        // Skipped while a post-pairing hydration burst is pulling.
        if initial_resync_pending() {
            if !hydration_burst_active() {
                info!("Startup sync incomplete; retrying watch-arm + resync on DHT readiness");
                spawn_startup_sync_retry();
            }
        } else {
            // Veilid can rearm a watch only if pinning succeeded previously.
            // Retry child pointers whose first pin failed after startup.
            spawn(async {
                child_watch::sync().await;
            });
        }
    }
}

/// One-shot flag so overlapping connectivity edges don't stack duplicate
/// startup-sync retries while one is in flight.
static STARTUP_SYNC_RETRY_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// RAII reset for [`STARTUP_SYNC_RETRY_IN_FLIGHT`].
struct StartupSyncRetryInFlight;

impl Drop for StartupSyncRetryInFlight {
    fn drop(&mut self) {
        STARTUP_SYNC_RETRY_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Retries the startup watch-arm + full resync off the update-worker loop:
/// both can block on DHT timeouts, and awaiting them inline would leave the
/// worker's shutdown/attach `select!` unpolled for that duration.
fn spawn_startup_sync_retry() {
    if STARTUP_SYNC_RETRY_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    spawn(async move {
        let _in_flight = StartupSyncRetryInFlight;
        nonfatal!(
            "veilid.core.updates",
            "Failed to re-arm identity watch on reconnect",
            start().await
        );
        resync_identity().await;
    });
}
