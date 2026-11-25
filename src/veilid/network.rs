//! Network status monitoring for offline-first applications.
//!
//! Tracks Veilid network connectivity and provides utilities for offline-aware operations.

use veilid_core::{AttachmentState, VeilidStateAttachment};

/// Network connectivity status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetworkStatus {
    /// Whether the node is currently attached to the network
    pub is_attached: bool,
    /// Quality of network attachment
    pub attachment_quality: AttachmentQuality,
    /// Whether public internet routing is ready
    pub public_internet_ready: bool,
    /// Whether local network routing is ready
    pub local_network_ready: bool,
    /// Reliable (gauge x4) peers in the routing table.
    pub reliable_peer_count: u32,
    /// Live (reliable + unreliable) peers in the routing table.
    pub live_peer_count: u32,
    /// Smoothed estimate of total reachable network size, `None` if not yet known.
    pub estimated_network_size: Option<u32>,
    /// Median p75 latency across reliable peers in milliseconds, `None` if no samples yet.
    pub median_latency_ms: Option<u64>,
    /// Uptime since the current attach began, in seconds, `None` while detached.
    pub attached_uptime_secs: Option<u64>,
}

/// Quality levels of network attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttachmentQuality {
    /// Startup/default state before Veilid has reported attachment details.
    #[default]
    Unknown,
    /// Node is not attached to the network.
    Offline,
    /// Node is in the process of attaching.
    Connecting,
    Weak,
    Good,
    Strong,
    FullyAttached,
}

impl From<AttachmentState> for AttachmentQuality {
    fn from(state: AttachmentState) -> Self {
        match state {
            AttachmentState::Detached | AttachmentState::Detaching => Self::Offline,
            AttachmentState::Attaching => Self::Connecting,
            AttachmentState::AttachedWeak => Self::Weak,
            AttachmentState::AttachedGood | AttachmentState::AttachedFair => Self::Good,
            AttachmentState::AttachedStrong => Self::Strong,
            AttachmentState::AttachedFull => Self::FullyAttached,
        }
    }
}

/// Snapshot of detailed network telemetry while attached, for the activity modal.
///
/// Cheap to copy so it can flow into Dioxus render memos without shared state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetworkDetailLine {
    pub reliable_peer_count: u32,
    pub live_peer_count: u32,
    pub estimated_network_size: Option<u32>,
    pub median_latency_ms: Option<u64>,
    pub attached_uptime_secs: Option<u64>,
}

fn saturating_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

const MICROS_PER_MILLI: u64 = 1_000;
const MICROS_PER_SEC: u64 = 1_000_000;

impl NetworkStatus {
    /// Create a new network status from Veilid attachment state.
    pub fn from_attachment(attachment: &VeilidStateAttachment) -> Self {
        Self {
            is_attached: attachment.state.is_attached(),
            attachment_quality: attachment.state.into(),
            public_internet_ready: attachment.public_internet_ready,
            local_network_ready: attachment.local_network_ready,
            reliable_peer_count: saturating_u32(attachment.reliable_peer_count.as_u64()),
            live_peer_count: saturating_u32(attachment.live_peer_count.as_u64()),
            estimated_network_size: Some(saturating_u32(
                attachment.estimated_network_size.as_u64(),
            )),
            median_latency_ms: attachment
                .median_latency
                .map(|latency| latency.as_u64() / MICROS_PER_MILLI),
            attached_uptime_secs: attachment
                .attached_uptime
                .map(|uptime| uptime.as_u64() / MICROS_PER_SEC),
        }
    }

    /// Whether Veilid can plausibly deliver traffic right now: attached with
    /// at least one live peer to talk to.
    ///
    /// A *transport* signal, not a DHT one. A node that has just attached
    /// loads its routing table from disk, so it reports live peers before the
    /// storage manager will serve a record. Use [`Self::dht_ready`] for
    /// anything that opens or reads a record.
    #[must_use]
    pub const fn has_connectivity(&self) -> bool {
        self.is_attached && self.live_peer_count > 0
    }

    /// Whether `veilid-core` will serve a DHT record operation right now.
    ///
    /// The storage manager gates every record open on its own `dht_is_online`
    /// flag, which it sets from the **`PublicInternet`** routing domain
    /// becoming ready in both directions (veilid-core `storage_manager/mod.rs`,
    /// `routing_domain_ready_event_handler`, whose comment pins it to "the
    /// same level-based signal as `public_internet_ready`"). While that flag is
    /// false, `open_dht_record` fails at once with
    /// `TryAgain("offline, try again later")` and sends no packet at all.
    ///
    /// [`Self::has_connectivity`] is strictly weaker and is not a substitute:
    /// in the window between attach and domain readiness the routing table
    /// already reports live peers, and a record open there can only fail —
    /// which reaches callers as an error indistinguishable from a missing
    /// record.
    ///
    /// The `LocalNetwork` domain does not count: the storage manager ignores
    /// its ready events, so LAN readiness alone cannot make an open succeed.
    #[must_use]
    pub const fn dht_ready(&self) -> bool {
        self.public_internet_ready
    }

    /// Build a detail snapshot for the activity modal, or `None` when detached.
    pub const fn attached_detail(&self) -> Option<NetworkDetailLine> {
        if !self.is_attached {
            return None;
        }
        Some(NetworkDetailLine {
            reliable_peer_count: self.reliable_peer_count,
            live_peer_count: self.live_peer_count,
            estimated_network_size: self.estimated_network_size,
            median_latency_ms: self.median_latency_ms,
            attached_uptime_secs: self.attached_uptime_secs,
        })
    }
}
