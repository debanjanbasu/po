//! Signal fanout and addressing helpers shared by the chat flows.

use std::{collections::HashSet, sync::Arc};

use veilid_core::RecordKey;

use crate::{
    helpers::concurrency, session::epoch as session, storage::background_errors::report_nonfatal,
    user::ctx::UserCtx, veilid::core::parse_record_key,
};

use super::{PeerSignal, prepare_signal_bytes, routes::send_prepared_signal};

/// Parses a conversation record key together with the current user's identity
/// key — the addressing pair every conversation-scoped signal needs.
/// Returns `None` when not logged in or either key is malformed.
pub fn conv_sender_keys(conv_key: &str) -> Option<(RecordKey, RecordKey)> {
    let conv = parse_record_key(conv_key).ok()?;
    let sender = UserCtx::try_current().and_then(|ctx| parse_record_key(&ctx.dht_key).ok())?;
    Some((conv, sender))
}

/// Sends a signal to every peer concurrently. Fire-and-forget.
pub fn signal_all_peers(peers: &[String], sig: &PeerSignal) {
    if peers.is_empty() {
        return;
    }
    let mut unique = Vec::with_capacity(peers.len());
    let mut seen = HashSet::with_capacity(peers.len());
    for peer in peers {
        if seen.insert(peer.as_str()) {
            unique.push(peer.clone());
        }
    }
    let total = unique.len();
    let sig = sig.clone();
    session::spawn_session_scoped(async move {
        let Some(message) = prepare_signal_bytes(&sig).await else {
            return;
        };
        // Durable signals may be re-sent after a failed fan-out; presence
        // signals must not (see `PeerSignal::is_durable`).
        let retryable = sig.is_durable();
        // Unbounded: one DHT round trip for the whole peer set instead of
        // `ceil(peers / MAX_SIGNAL_FANOUT)` waves of it; each send keeps its
        // own per-route timeout inside `send_prepared_signal`.
        let sends = concurrency::map_concurrent(unique, {
            let message = &message;
            async move |peer: String| {
                send_prepared_signal(&peer, Arc::clone(message), None, retryable).await
            }
        })
        .await;
        let failed = u32::try_from(sends.iter().filter(|delivered| !**delivered).count())
            .unwrap_or(u32::MAX);

        if failed > 0 {
            report_nonfatal(
                "chat.signal",
                "Realtime delivery degraded",
                format!("{failed}/{total} peers unreachable — message queued for DHT sync"),
            );
        }
    });
}
