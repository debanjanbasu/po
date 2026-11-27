//! Outbound signal sending: receiver-targeted preparation, direct sends, and
//! broadcast/excluding fan-out over the remote-route cache.

use std::{sync::Arc, time::Duration};

use anyhow::{Result, bail};
use dioxus::logger::tracing::{debug, warn};
use futures_util::{StreamExt, stream::FuturesUnordered};
use veilid_core::{KeyPair, RecordKey, RouteId, RoutingContext, Target};

use crate::{
    chat::signal::PeerSignal,
    config,
    helpers::async_utils::timeout,
    user::ctx::UserCtx,
    veilid::core::{get_veilid_api, parse_record_key},
};

use super::routes_remote::{evict_routes_for_identity, resolve_peer_routes};

async fn prepare_outbound_signal_for_sender(
    signal: &PeerSignal,
    sender_identity: &RecordKey,
    keypair: &KeyPair,
) -> Result<Vec<u8>> {
    if signal.has_sender_proof() {
        return Ok(signal.to_bytes());
    }
    if signal.sender_identity() != sender_identity {
        bail!("mismatched sender identity");
    }
    signal.to_signed_bytes(keypair).await
}

pub async fn prepare_signal_bytes(signal: &PeerSignal) -> Option<Arc<Vec<u8>>> {
    if signal.has_sender_proof() {
        return Some(Arc::new(signal.to_bytes()));
    }
    let Some(ctx) = UserCtx::try_current() else {
        warn!(
            "Cannot send {} signal without an active user context",
            signal.kind_name(),
        );
        return None;
    };
    let Some(keypair) = ctx.keypair.as_ref() else {
        warn!(
            "Cannot send {} signal without a local keypair",
            signal.kind_name(),
        );
        return None;
    };
    let Ok(sender_identity) = parse_record_key(ctx.dht_key.as_ref()) else {
        warn!(
            "Cannot send {} signal with an invalid local sender identity",
            signal.kind_name(),
        );
        return None;
    };
    prepare_signal_bytes_as(signal, &sender_identity, keypair).await
}

pub(in crate::chat::signal) async fn prepare_signal_bytes_as(
    signal: &PeerSignal,
    sender_identity: &RecordKey,
    keypair: &KeyPair,
) -> Option<Arc<Vec<u8>>> {
    match prepare_outbound_signal_for_sender(signal, sender_identity, keypair).await {
        Ok(message) => Some(Arc::new(message)),
        Err(e) => {
            warn!(
                "Cannot send {} signal with the requested sender identity: {e}",
                signal.kind_name(),
            );
            None
        }
    }
}

pub async fn send_signal_as(
    identity_key: &str,
    signal: &PeerSignal,
    sender_identity: &RecordKey,
    keypair: &KeyPair,
) -> bool {
    let Some(message) = prepare_signal_bytes_as(signal, sender_identity, keypair).await else {
        return false;
    };
    send_prepared_signal(identity_key, message, None, signal.is_durable()).await
}

/// Sends a prepared signal while skipping one device's route — `Some` for
/// same-account sync nudges so the sending device does not signal itself.
pub(in crate::chat::signal) async fn send_prepared_signal(
    identity_key: &str,
    message: Arc<Vec<u8>>,
    exclude_device: Option<&str>,
    retryable: bool,
) -> bool {
    let Ok(api) = get_veilid_api().await else {
        return false;
    };
    let Ok(rc) = api.routing_context() else {
        return false;
    };
    let identity_key: Arc<str> = Arc::from(identity_key);
    let rc = Arc::new(rc);

    // A peer that restarted without republishing keeps being re-imported from
    // a fresh-but-dead blob for CACHE_TTL_MS, so one failed fan-out is not
    // proof the peer is gone. Durable signals get a single re-resolve (the
    // cache was just evicted) and re-send; presence signals do not, because a
    // replay would resurrect an indicator the peer already cleared.
    let mut remaining_attempts = if retryable { 2 } else { 1 };
    loop {
        remaining_attempts -= 1;
        let routes = resolve_peer_routes(identity_key.as_ref(), exclude_device).await;
        if routes.is_empty() {
            // Exclusion can leave no target without making the cached routes stale.
            debug!("Signal undeliverable: no live routes for {identity_key}");
            return false;
        }
        if send_over_routes(&identity_key, &rc, routes, &message).await {
            return true;
        }
        // All routes failed — evict cache so next attempt re-resolves from DHT.
        evict_routes_for_identity(identity_key.as_ref()).await;
        if remaining_attempts == 0 {
            return false;
        }
    }
}

/// Sends `message` to every resolved route, concurrently, and returns whether
/// at least one route accepted it.
async fn send_over_routes(
    identity_key: &Arc<str>,
    rc: &Arc<RoutingContext>,
    routes: Vec<RouteId>,
    message: &Arc<Vec<u8>>,
) -> bool {
    let send_timeout = Duration::from_millis(config::route::SEND_TIMEOUT_MS);
    // Unbounded across routes: every route gets its own send with its own
    // per-route timeout, so the fan-out costs one round trip, not
    // `ceil(routes / budget)` waves of them.
    let mut sends = FuturesUnordered::new();
    for route_id in routes {
        let identity_key = Arc::clone(identity_key);
        let message = Arc::clone(message);
        let rc = Arc::clone(rc);
        sends.push(async move {
            let target = Target::RouteId(route_id);
            match timeout(
                send_timeout,
                rc.app_message(target, message.as_ref().clone()),
            )
            .await
            {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    warn!("Route send failed for {identity_key}: {e}");
                    false
                }
                Err(_) => {
                    warn!("Route send timed out for {identity_key}");
                    false
                }
            }
        });
    }

    let mut any_ok = false;
    while let Some(delivered) = sends.next().await {
        any_ok |= delivered;
    }
    any_ok
}
