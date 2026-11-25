//! Direct peer-to-peer signaling via Veilid `app_message`.
//!
//! Ephemeral signals (typing, read hints, data nudges) are sent directly
//! to the peer's private route for instant delivery. Conversation-scoped
//! variants carry sender proofs because private-route `app_message`s do not
//! expose a trusted sender identity under Veilid's default safety mode.
//! DHT remains the durable fallback — signals are fire-and-forget
//! accelerators.
//! See also: docs/peer-signaling.md.

mod dispatch;
mod fanout;
mod nudge;
mod routes;
mod wire;

pub use dispatch::handle_app_message;
pub use fanout::{conv_sender_keys, signal_all_peers};
pub use nudge::nudge_sibling_devices;
pub use routes::{
    clear_route_cache, evict_routes_for_identity, handle_dead_routes, prepare_signal_bytes,
    publish_local_route, release_local_route, send_signal_as, start_route_heartbeat,
};
pub use wire::{PeerSignal, SignalPayload};
