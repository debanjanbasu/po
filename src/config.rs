//! Centralized configuration constants.
//!
//! Tuning knobs, size limits, and timing values live here instead of being
//! scattered across individual modules. Import from `crate::config::*`.

// ── App identity ─────────────────────────────────────────────────────

pub mod app {
    /// Reverse-DNS application identifier (`Dioxus.toml` mirrors this).
    pub const APP_ID: &str = "com.debanjanbasu.po";
}

// ── Time helpers ─────────────────────────────────────────────────────

/// One hour in milliseconds — reusable constant for interval calculations.
pub const ONE_HOUR_MS: u64 = 3_600_000;

/// One hour in seconds.
pub const ONE_HOUR_SECS: u64 = 3600;

// ── Validation ───────────────────────────────────────────────────────

pub mod validation {
    /// Minimum username length.
    pub const USERNAME_MIN_LEN: usize = 3;
    /// Maximum username length.
    pub const USERNAME_MAX_LEN: usize = 256;
    /// Minimum password length.
    pub const PASSWORD_MIN_LEN: usize = 8;
    /// Maximum password length.
    pub const PASSWORD_MAX_LEN: usize = 256;
    /// Minimum PIN length (user PIN, not pairing).
    pub const PIN_MIN_LEN: usize = 6;
    /// Exact pairing PIN length (12 decimal digits, roughly 40 bits of entropy).
    pub const PAIRING_PIN_EXACT_LEN: usize = 12;
    /// Maximum human age in years for DOB validation.
    pub const MAX_AGE_YEARS: i32 = 150;
    /// Minimum age in years for DOB validation.
    pub const MIN_AGE_YEARS: i32 = 13;
}

// ── Crypto ───────────────────────────────────────────────────────────

pub mod crypto {
    use veilid_core::CryptoKind;

    /// Standard cryptographic key length (Veilid uses 32-byte keys).
    /// Not exported as `pub const` by veilid-core; only available at runtime
    /// via `CryptoSystemGuard::shared_secret_length()`.
    pub const KEY_LEN: usize = 32;
    /// `CryptoKind` tag length on the wire — derived from the actual type size.
    pub const CRYPTO_KIND_LEN: usize = size_of::<CryptoKind>();
    /// Veilid `RecordKey` wire size: kind(4) + key(32).
    pub const RECORD_KEY_WIRE_LEN: usize = CRYPTO_KIND_LEN + KEY_LEN;
}

// ── Pairing ──────────────────────────────────────────────────────────

pub mod pairing {
    use std::time::Duration;

    /// How long a pairing payload stays valid (5 minutes).
    pub const TTL_SECS: u64 = 300;
    /// Maximum base64-encoded payload size accepted.
    pub const MAX_BASE64_PAYLOAD_LEN: usize = 1024;
    /// Minimum decoded pairing payload length.
    pub const MIN_PAYLOAD_LEN: usize = 32;
    /// V2 plaintext layout: version(1) + expires(8) + kind(4) + dht(32) +
    /// enc-flag(1) + enc-key(32) + pub(32) + sec(32). The DHT record's
    /// embedded encryption key must travel with the opaque key: without it a
    /// paired device can route to the record but cannot decrypt its values
    /// (Veilid encrypts record values at rest with this key).
    pub const V2_PLAINTEXT_LEN: usize = 1 + 8 + 4 + 32 + 1 + 32 + 32 + 32;
    /// Payload length after stripping the version byte.
    pub const V2_PAYLOAD_LEN: usize = V2_PLAINTEXT_LEN - 1;
    /// Maximum device-label bytes carried in a pairing beacon.
    pub const BEACON_MAX_LABEL_BYTES: usize = 48;
    /// Clock-skew allowance when filtering stale beacons by timestamp.
    pub const BEACON_TIMESTAMP_SLACK_SECS: u64 = 120;
    /// Retry delays for a beacon milestone write that hits a transient
    /// network error (`TryAgain`/offline). Milestones are the share device's
    /// only view of pairing progress, so a write lost to a flapping network
    /// is worth a few short retries — but bounded, since the joining device
    /// keeps syncing regardless.
    pub const BEACON_WRITE_RETRY_WAITS_MS: [u64; 3] = [2_000, 5_000, 10_000];
    /// Budget for the joining device's DHT readiness gate before identity
    /// sync. Mobile bootstrap (attach, dial-info detection, routing-table
    /// fill, safety-route tests) realistically takes 15-60 s.
    pub const DHT_READY_BUDGET_MS: u64 = 60_000;
    /// Total budget for the joining device's identity fetch. A just-attached
    /// node's DHT fanouts keep failing until its routing table matures, so
    /// pairing retries far longer than the steady-state watcher sync.
    /// Readiness gate (60 s) + this stays within the QR's 5-minute life.
    pub const IDENTITY_SYNC_TIMEOUT: Duration = Duration::from_secs(180);
    /// Wait before each identity read round (ms). Escalating gaps give the
    /// routing table time to mature between fanouts instead of burning every
    /// round on the same immature network view.
    pub const SYNC_ROUND_WAITS_MS: [u64; 4] = [0, 5_000, 15_000, 30_000];
    /// Waits between re-pulls when the identity record is reachable but its
    /// profile/security predate onboarding — i.e. the sharing device's
    /// post-onboarding publish hasn't landed on the DHT yet and its sync
    /// queue is still retrying. Front-loaded (a healthy source lands within
    /// seconds), ~2 minutes total, within the QR's 5-minute life.
    pub const STALE_SOURCE_REPULL_WAITS_MS: [u64; 10] = [
        2_000, 3_000, 5_000, 5_000, 10_000, 10_000, 15_000, 15_000, 20_000, 30_000,
    ];
    /// Overall timeout for verifying (and repairing) network publication of
    /// the identity record before sharing a pairing QR.
    pub const PUBLISH_ENSURE_TIMEOUT: Duration = Duration::from_secs(120);
    /// Verification rounds after a repair write.
    pub const PUBLISH_VERIFY_ROUNDS: u64 = 3;
    /// Base delay between publish-verification rounds (scales linearly).
    pub const PUBLISH_VERIFY_BACKOFF_MS: u64 = 4_000;
    /// Post-expiry grace: how long the sharing device keeps showing sync
    /// progress after the QR's TTL lapses while the paired device is still
    /// mid-sync. Caps a stuck beacon from holding the UI forever.
    pub const SHARE_SYNC_HOLD_GRACE_SECS: u64 = 600;
    /// How long "Synced ✓" stays visible on the sharing device before the
    /// share UI resets.
    pub const SHARE_COMPLETE_LINGER_SECS: u64 = 6;
    /// How long the joined device keeps its completion state visible before
    /// the pairing UI resets (joining-side counterpart of
    /// `SHARE_COMPLETE_LINGER_SECS`).
    pub const JOIN_COMPLETE_LINGER_SECS: u64 = 4;
}

// ── PIN gate / lockout ───────────────────────────────────────────────

pub mod pin_gate {
    /// Attempts before the first cooldown window triggers.
    pub const LOCKOUT_THRESHOLD: u32 = 5;
    /// Base cooldown duration in seconds (scales exponentially).
    pub const COOLDOWN_SECS: u64 = 30;
    /// Hard cap on cooldown duration (1 hour) — never permanently locked out.
    pub const MAX_COOLDOWN_SECS: u64 = super::ONE_HOUR_SECS;
}

// ── Chat / conversation ──────────────────────────────────────────────

pub mod chat {
    /// Maximum raw contact-input length accepted (deep-link, paste, QR).
    pub const MAX_CONTACT_INPUT_LEN: usize = 4096;
    /// Maximum chars in a conversation preview snippet.
    pub const PREVIEW_MAX_CHARS: usize = 50;
    /// Maximum automerge payload size before archive rotation.
    pub const PAGE_SIZE_THRESHOLD: usize = 24_000;
    /// Maximum data subkeys in an archive DFLT(16) record (subkey 0 = index).
    pub const ARCHIVE_MAX_PAGES: u16 = 15;
    /// Maximum size in bytes of a single outgoing chat message's content.
    /// The size that matters is `estimated_message_size`'s content-byte count,
    /// not a char count: UTF-8 multi-byte characters (emoji are 4 bytes) made
    /// a char-based cap bypassable by up to 4x. Veilid rejects any subkey
    /// value over 32768 bytes (`ValueData::MAX_LEN`), and an oversized
    /// message that fails to write would wedge the conversation's head page
    /// forever. 16k bytes + ~200 bytes of fixed encoding overhead leaves
    /// margin for zstd expansion in the worst case (high-entropy content that
    /// fails to compress), keeping the serialized feed message under the
    /// ceiling even before page batching.
    pub const MAX_MESSAGE_BYTES: usize = 16_000;
    /// Safety bound for archive chain traversal.
    pub const MAX_ARCHIVE_CHAIN_DEPTH: usize = 64;
    /// How many messages the conversation view renders at a time.
    ///
    /// The view renders the newest slice of the history and grows it when the
    /// reader scrolls up, so a 2,000-message conversation does not put 2,000
    /// bubbles in the DOM at once. The window is a page size, not a cap:
    /// scrolling up loads another `WINDOW_MESSAGES` older messages, so the
    /// full history is still reachable.
    pub const WINDOW_MESSAGES: usize = 60;
    /// How many older messages one scroll-up step adds to the window.
    ///
    /// The step is a constant page size, so reaching the top of a long
    /// history takes one step per page. The DOM stays bounded by the
    /// window size, which grows by one page per step.
    pub const WINDOW_PAGE: usize = 60;
    /// Safety valve on session-owned conversation watches. When the
    /// `ConversationIndex` exceeds this, only the most recently active
    /// conversations are watched; the rest still sync via login catch-up
    /// and overflow resync.
    pub const MAX_CONVERSATION_WATCHES: usize = 64;
    /// Overall timeout for post-pairing conversation hydration (seconds).
    pub const HYDRATION_TIMEOUT_SECS: u64 = 120;
    /// Maximum messages a conversation keeps in local storage and cache.
    /// `persist_locked` drops the oldest messages past this, never a
    /// [`MessageStatus::Pending`](crate::chat::models::feed::MessageStatus::Pending)
    /// one and never one at or after `my_feed_meta.head_page_start_seq` — the
    /// exact set [`build_head_page`](crate::chat::record::feed_sync) republishes,
    /// so everything dropped is already in an archive chain the backfill walks.
    ///
    /// Two accepted costs: the UI can no longer display history beyond the cap
    /// (it is only on the archive chain, refetched on demand), and
    /// pointer-keyed `Arc` clones of dropped messages' `msg_id`s are gone — the
    /// merge path no longer memoizes dedup keys, so nothing pins those
    /// allocations. Peer messages below this boundary are only recoverable
    /// from the sender's record while they remain a member. Note the cap does
    /// not quiet `archive_backfill_needed`: the dropped prefix reads as a
    /// permanent gap, so archive-touching rounds keep re-merging the chain
    /// (cheap per round, unbounded over time).
    pub const MAX_LOCAL_MESSAGES: usize = 2_000;
    /// Exact shape of a `msg_id` a remote page may carry.
    ///
    /// [`generate_msg_id`](crate::chat::models::feed::generate_msg_id) emits 32
    /// lowercase hex characters, and a remote page's id must match that shape
    /// exactly: the merge path keys dedup on the id's bytes, so an exact shape
    /// keeps two distinct messages from aliasing one dedup key (`"+10"`-style
    /// variants of an id, uppercase hex, zero-padding), and a page can no longer
    /// pin megabytes of string under the merge. Anything else is refused at the
    /// wire (`normalize_remote_messages`).
    pub const MSG_ID_HEX_LEN: usize = 32;
}

// ── Typing indicators ────────────────────────────────────────────────

pub mod typing {
    /// Typing indicator expiry (5 seconds).
    pub const EXPIRY_MS: u64 = 5000;
    /// Typing indicator considered stale after this (2× expiry).
    pub const STALE_MS: u64 = EXPIRY_MS * 2;
}

// ── Sync queue ───────────────────────────────────────────────────────

pub mod sync {
    /// Maximum retry attempts before dead-letter.
    pub const MAX_RETRIES: u8 = 5;
    /// Maximum rows the dead-letter lane retains.
    ///
    /// Every other store has a ceiling (snapshots, pending invites, background
    /// errors); the dead-letter lane is the one that would otherwise grow
    /// forever: a row lands here only after its retry budget is spent, and
    /// nothing removes it until the user retries/dismisses it or the account is
    /// deleted — while `retry_all_dead_letters` re-enqueues the whole lane,
    /// with a fresh retry budget, on every DHT-ready edge.
    ///
    /// Passing the cap evicts the *oldest* rows through the regular dismiss
    /// path. That is the trade: an evicted row's "gave up" notice and its
    /// Retry/Dismiss affordance disappear from the queue UI, so a failure the
    /// user could still have acted on becomes invisible (any message the row
    /// carried keeps the failed status its dead-letter transition gave it).
    /// The number sits far above any genuine accumulation, so eviction only
    /// bites a lane that is failing systematically.
    pub const MAX_DEAD_LETTERS: usize = 100;
    /// Base delay for exponential backoff (1 second).
    pub const BACKOFF_BASE_MS: u64 = 1_000;
    /// Maximum backoff delay (1 hour).
    pub const MAX_BACKOFF_MS: u64 = super::ONE_HOUR_MS;

    /// Invite signal retry: base delay (5 seconds).
    ///
    /// The invite lands only while the peer is online, so the first retries
    /// must be quick enough to catch a peer that is momentarily unreachable
    /// (a network flap, a route that has not settled yet). A one-minute
    /// base meant the *first* retry waited two minutes — long past any
    /// interactive session.
    pub const INVITE_BACKOFF_BASE_MS: u64 = 5_000;
    /// Invite signal retry: maximum backoff (2 hours).
    pub const INVITE_MAX_BACKOFF_MS: u64 = 2 * super::ONE_HOUR_MS;
    /// Invite signal retry attempts before dead-letter.
    pub const INVITE_MAX_RETRIES: u8 = 12;
    /// Wall-clock budget for a delivered-but-unacked invite.
    ///
    /// Re-sending an invite the peer already holds is not a delivery attempt,
    /// so it is capped by elapsed time rather than by [`INVITE_MAX_RETRIES`]
    /// (which would dead-letter the lane while the peer is online but slow to
    /// ack). After this the lane does dead-letter, for manual recovery.
    pub const INVITE_ACK_WAIT_MS: u64 = 24 * super::ONE_HOUR_MS;
    /// Wall-clock patience for an op deferred by a pending password rotation.
    ///
    /// A mid-rotation op is not a delivery failure — the key journal must
    /// finish migrating before ordinary publication can land — so, like a
    /// delivered-but-unacked invite, the lane is capped by elapsed time
    /// rather than by [`MAX_RETRIES`], which would dead-letter it with a
    /// "gave up" notice while the rotation was still converging. The rotation
    /// op itself (`ReencryptSubkeys`) is immortal and drives that
    /// convergence; past this patience the blocked lane does dead-letter, for
    /// the DLQ retry path. Waiting is cheap: the lane still backs off on
    /// [`BACKOFF_BASE_MS`], so it re-reads the queue a few times an hour
    /// rather than once a second.
    pub const ROTATION_WAIT_MS: u64 = 24 * super::ONE_HOUR_MS;
}

// ── Routes / peer signaling ──────────────────────────────────────────

pub mod route {
    /// Maximum age (ms) of a route entry before it is considered stale.
    pub const FRESHNESS_MS: u64 = 120_000;
    /// Cadence (ms) of the live-route heartbeat that re-stamps this
    /// device's route entry. Must be well under [`FRESHNESS_MS`] so one
    /// failed beat still leaves the entry fresh (45s ≈ 2.6 beats per
    /// freshness window).
    pub const HEARTBEAT_MS: u64 = 45_000;
    /// Per-route send attempt timeout (ms). Fan-out is concurrent, so this
    /// bounds tail latency, not throughput. Relayed/NATted paths routinely
    /// exceed 500ms — keep enough headroom that a slow-but-alive device
    /// still gets its nudge.
    pub const SEND_TIMEOUT_MS: u64 = 2_000;
    /// Maximum route entries stored in a single subkey. Well within the
    /// 32 KB DHT value limit (~300 bytes per entry × 16 = ~5 KB).
    pub const MAX_ENTRIES: usize = 16;
    /// Private route allocation timeout (ms). On hostile networks
    /// `new_private_route` can hang indefinitely compiling/testing a
    /// route; an unbounded attempt wedges the publish in-flight flag and
    /// silently starves the heartbeat. Bounded attempts fail fast and
    /// retry via backoff + the heartbeat's full-publish fallback.
    pub const CREATE_TIMEOUT_MS: u64 = 15_000;
    /// Route publish: attempts before surfacing an error. Right after
    /// startup/reconnect the DHT often rejects writes until it warms up,
    /// and at session start there is no identity record to write to until
    /// onboarding's `CreateIdentity` op lands. The budget has to outlast
    /// both (2s → 4s → 8s → 16s → 32s between six attempts, ~1 min worst
    /// case).
    pub const PUBLISH_MAX_ATTEMPTS: u32 = 6;
    /// Route publish: initial retry backoff (ms); doubles per attempt
    /// (2s → 4s → 8s → 16s → 32s).
    pub const PUBLISH_RETRY_BASE_MS: u64 = 2_000;
    /// Cache TTL for resolved peer routes (ms).
    pub const CACHE_TTL_MS: u64 = 120_000;
}

// ── Same-account sync nudge ──────────────────────────────────────────

pub mod nudge {
    //! Scope bits carried by the `SyncNudge` peer signal (wire contract —
    //! only add bits, never reassign).

    /// Identity core subkeys changed (profile / security / app state).
    pub const SCOPE_CORE: u8 = 0x01;
    /// TOTP child record changed.
    pub const SCOPE_TOTP: u8 = 0x02;
    /// Contacts child record changed.
    pub const SCOPE_CONTACTS: u8 = 0x04;
    /// Conversation index child record changed.
    pub const SCOPE_CONVERSATIONS: u8 = 0x08;
    /// Compression dictionary subkeys changed.
    pub const SCOPE_DICT: u8 = 0x10;
    /// All subkeys rewritten (password change) — full resync.
    pub const SCOPE_FULL: u8 = 0x20;
    /// Delay before the single re-pull retry when a nudged pull saw no
    /// changes (covers the sender's DHT write still propagating).
    pub const RETRY_DELAY_MS: u64 = 2_000;
}

// ── App lifecycle sync hooks ─────────────────────────────────────────

pub mod lifecycle {
    /// Foreground catch-up: resync when no verified sync contact with the
    /// network happened for this long (watch events, nudges, clean polls).
    pub const STALE_AFTER_MS: u64 = 30_000;
}

// ── Inspect poll (Tier-3 sync backstop) ──────────────────────────────

pub mod inspect_poll {
    /// Fast cadence (ms) right after a poll caught a missed change —
    /// push paths look unhealthy, so check more often.
    pub const FAST_MS: u64 = 15_000;
    /// Slow cadence (ms) once push paths look healthy.
    pub const SLOW_MS: u64 = 60_000;
    /// Consecutive clean polls before slowing back down.
    pub const CLEAN_POLLS_TO_SLOW: u32 = 3;
}

// ── Change dispatcher ────────────────────────────────────────────────

pub mod dispatcher {
    /// Channel buffer capacity for coalescing DHT value changes.
    pub const CHANNEL_CAPACITY: usize = 256;
    /// Time window (ms) for coalescing burst events before dispatching.
    /// Only applied when a key already has in-flight or pending state;
    /// fresh keys dispatch immediately with zero added latency.
    pub const COALESCE_WINDOW_MS: u64 = 25;
}

// ── Compression ──────────────────────────────────────────────────────

pub mod compression {
    /// zstd level for transport and structured codec operations. 22 is
    /// byte-identical to 19 at up to 46% more CPU; 12 costs ~1-3% more bytes
    /// but ~1/9 the time, keeping publishes off the UI thread (web runs the
    /// codec inline).
    ///
    /// Every measured table in `docs/compression.md` (and the copy of it in
    /// `storage/automerge.rs`) predates this value: those are native **level
    /// 22** numbers, so read them as CPU ceilings, not as shipped behaviour.
    pub const ZSTD_LEVEL: i32 = 12;
    /// Maximum dictionary versions kept in memory.
    pub const MAX_DICT_VERSIONS: usize = 8;
    /// Maximum decompressed size for any single blob (10 MB).
    pub const MAX_DECOMPRESSED_BYTES: usize = 10 * 1024 * 1024;
    /// Minimum samples before training can occur.
    pub const MIN_TRAINING_SAMPLES: usize = 1000;
    /// Minimum interval between training runs.
    pub const MIN_TRAINING_INTERVAL_SECS: u64 = super::ONE_HOUR_SECS;
    /// Dictionary size for training.
    pub const DICT_SIZE: usize = 16_384;
    /// Minimum improvement percentage to swap dictionary.
    pub const MIN_IMPROVEMENT_PERCENT: f64 = 1.0;
    /// Recompression check interval.
    pub const RECOMPRESSION_INTERVAL_MS: u64 = super::ONE_HOUR_MS;
    /// How soon a heavy pass that *stopped short* asks to be resumed — the
    /// recompression cycle (deferred while a web page is visible, or stopped
    /// early) and dictionary training (same deferral).
    ///
    /// Distinct from the intervals above, which are the idle cadences. A pass
    /// that has work to do and cannot do it *now* must not wait an hour: on a
    /// tab the user keeps open and visible, an hour-long wait is a pass that
    /// never runs. One minute is short enough to catch the next hidden moment
    /// and long enough that the check itself — an in-memory compare plus one
    /// marker read, and no scan — costs nothing.
    pub const DEFERRED_RESUME_MS: u64 = 60_000;
    /// Maximum entries in each LRU sample cache.
    pub const SAMPLE_LRU_CAPACITY: usize = 10_000;
    /// Threshold of pending samples before automatic flush.
    pub const SAMPLE_FLUSH_THRESHOLD: usize = 100;
    /// Maximum n-gram length for word sequences.
    pub const MAX_NGRAM_LEN: usize = 3;
}

// ── Backup export/import ─────────────────────────────────────────────

pub mod backup {
    /// Payloads at or above this size use multithreaded zstd compression
    /// (`zstdmt`). Below it, MT job-splitting has nothing to parallelize.
    /// (Native only — wasm has no zstd worker threads.)
    #[cfg(not(target_arch = "wasm32"))]
    pub const MT_THRESHOLD_BYTES: usize = 8 * 1024 * 1024;
    /// Maximum worker threads for multithreaded backup compression.
    /// (Native only — wasm has no zstd worker threads.)
    #[cfg(not(target_arch = "wasm32"))]
    pub const MT_MAX_WORKERS: u32 = 4;
    /// Upper bound for a decompressed backup payload. Guards allocation
    /// before decompressing an imported file.
    ///
    /// On web it is far smaller, deliberately. Import holds several whole
    /// copies at once — the picked file, a ciphertext copy, the decrypted
    /// frame, the decompressed payload, then one copy per section (≈
    /// `2 × file + 2 × plaintext`) — inside a wasm heap that never shrinks once
    /// it grows, and the failure there is a trap with no message: the build is
    /// `panic = "immediate-abort"` with `build-std`, i.e. a dead tab. 32 MiB
    /// caps that peak near 130 MB, which a tab absorbs. Export refuses the same
    /// size with a sentence naming the limit (`storage::backup::ExportError`),
    /// which is a better product than an out-of-memory tab.
    #[cfg(target_arch = "wasm32")]
    pub const MAX_DECOMPRESSED_BYTES: u64 = 32 * 1024 * 1024;
    #[cfg(not(target_arch = "wasm32"))]
    pub const MAX_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
    /// Maximum sections in a backup payload. There are 5 known section
    /// types; the cap bounds allocation from attacker-controlled counts.
    pub const MAX_SECTIONS: usize = 16;
    /// Maximum items in a keyed-list section (per-conversation rows).
    /// Bounds allocation from attacker-controlled counts.
    pub const MAX_KV_ITEMS: usize = 100_000;
    /// Upper bound for an imported backup file itself, checked before the
    /// file is read into memory. Slightly above `MAX_DECOMPRESSED_BYTES`
    /// to cover zstd + AEAD overhead on incompressible payloads.
    ///
    /// Web mirrors the same relationship at its own scale: 34 MiB for a 32 MiB
    /// payload cap. Both are checked before the file is read
    /// (`components/settings/snapshots/import.rs`) and again in `parse_header`.
    #[cfg(target_arch = "wasm32")]
    pub const MAX_FILE_BYTES: u64 = 34 * 1024 * 1024;
    #[cfg(not(target_arch = "wasm32"))]
    pub const MAX_FILE_BYTES: u64 = 272 * 1024 * 1024;
}

// ── Storage / snapshots ──────────────────────────────────────────────

pub mod storage {
    /// Default compaction threshold in **raw (uncompressed) doc bytes**
    /// (16 KB). The doc row itself is stored automerge-DEFLATE, so a stored
    /// length would measure a different unit than this threshold.
    pub const COMPACTION_THRESHOLD: usize = 16_384;
    /// Maximum number of snapshots to keep per model per user.
    pub const MAX_SNAPSHOTS: usize = 100;
    /// Maximum background errors retained.
    pub const MAX_BACKGROUND_ERRORS: usize = 20;
    /// `TableDB` name for per-device identity and label rows.
    pub const DEVICE_TABLE: &str = "oo_device";
}

// ── DHT / Veilid ─────────────────────────────────────────────────────

pub mod dht {
    use std::time::Duration;

    /// Default timeout for DHT record operations.
    pub const RECORD_TIMEOUT: Duration = Duration::from_secs(30);

    /// Timeout for full identity sync in the steady-state watcher: covers
    /// multiple retry rounds of subkey reads. Pairing uses the much larger
    /// `config::pairing::IDENTITY_SYNC_TIMEOUT` (180 s) because a
    /// just-attached node's fanouts keep failing until its routing table
    /// matures, while the watcher runs on a warm routing table.
    pub const IDENTITY_SYNC_TIMEOUT: Duration = Duration::from_secs(60);

    /// Wait before each identity read round (ms) for the steady-state
    /// watcher sync: retry quickly, the routing table is already warm.
    pub const WATCHER_ROUND_WAITS_MS: [u64; 3] = [0, 750, 1_500];
}

// ── QR scanner ───────────────────────────────────────────────────────

// Used by platform-specific scanner implementations (Linux, Windows, WASM).
#[cfg(any(target_arch = "wasm32", target_os = "linux", target_os = "windows"))]
pub mod qr {
    /// Scanner timeout duration in seconds.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub const SCAN_TIMEOUT_SECS: u64 = 60;
    /// BT.601 luma coefficient for red channel.
    #[cfg(target_arch = "wasm32")]
    pub const LUMA_R: u16 = 77;
    /// BT.601 luma coefficient for green channel.
    #[cfg(target_arch = "wasm32")]
    pub const LUMA_G: u16 = 150;
    /// BT.601 luma coefficient for blue channel.
    #[cfg(target_arch = "wasm32")]
    pub const LUMA_B: u16 = 29;
}

// ── UI timings ───────────────────────────────────────────────────────

pub mod ui {
    /// Success feedback auto-dismiss (settings forms, TOTP toasts).
    pub const SUCCESS_TOAST_MS: u64 = 3_000;
    /// Error toast auto-dismiss — longer so the message can be read.
    pub const ERROR_TOAST_MS: u64 = 8_000;
    /// Deep-link toast auto-dismiss.
    pub const DEEP_LINK_TOAST_MS: u64 = 5_000;
    /// Inline "Copied ✓" feedback duration on QR share-card buttons.
    pub const SHARE_FEEDBACK_MS: u64 = 2_000;
    /// Share-fallback notice duration (clipboard/text-only fallback note).
    pub const SHARE_NOTICE_MS: u64 = 5_000;
}

// ── Deep links ───────────────────────────────────────────────────────

pub mod deep_link {
    /// URL scheme prefix for chat deep links.
    pub const CHAT_SCHEME: &str = "po://chat?";
    /// URL scheme prefix for pairing deep links.
    pub const PAIR_SCHEME: &str = "po://pair?";
    /// URL scheme prefix for the harness-only send action
    /// (`po://send?to=<username>&text=<text>`). Parsed only while
    /// [`e2e::enabled`] is on, so a shipped build ignores the link.
    pub const SEND_SCHEME: &str = "po://send?";
}

// ── Harness seams (opt-in, never distributed) ───────────────────────

/// Knobs the tandem device-test harness uses ([docs/device-testing.md]).
///
/// Honoured by debug builds and by any build that enables the off-by-default
/// `e2e-seams` feature (a release-profile harness build); every other build
/// compiles the accessors to a no-op, so a shipped binary can neither relocate
/// its data nor accept pairing credentials from the environment.
pub mod e2e {
    /// Reads a seam value from whichever channel this platform has.
    ///
    /// The environment is the channel on desktop and iOS. Android cannot set
    /// environment variables for an app started by `am start`, so the harness
    /// carries the same values in the launch intent's extras
    /// (`po_e2e_<KEY>`), which `MainActivity` hands to Rust over JNI.
    ///
    /// Seam builds only.
    #[cfg(any(debug_assertions, feature = "e2e-seams"))]
    fn seam_value(key: &str) -> Option<String> {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        #[cfg(target_os = "android")]
        if let Some(value) = crate::platform::native_bridge::e2e_seam(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        None
    }

    /// `PO_E2E=1` — this process is under the harness: emit the marker lines
    /// the harness tails.
    ///
    /// Native reads the environment. The browser has no environment channel —
    /// `std::env::var` is a stub there that always reports "not present" — so
    /// the harness sets `localStorage.po_e2e = "1"` instead. Storage, not a
    /// query parameter: a query parameter does not survive the app's in-app
    /// navigation, and `enabled()` is read at event time, not only at boot.
    #[cfg(all(
        any(debug_assertions, feature = "e2e-seams"),
        not(target_arch = "wasm32")
    ))]
    #[must_use]
    pub fn enabled() -> bool {
        seam_value("PO_E2E").is_some_and(|value| value == "1")
    }

    #[cfg(all(any(debug_assertions, feature = "e2e-seams"), target_arch = "wasm32"))]
    #[must_use]
    pub fn enabled() -> bool {
        web_sys::window()
            .and_then(|window| window.local_storage().ok().flatten())
            .and_then(|store| store.get_item("po_e2e").ok().flatten())
            .is_some_and(|value| value == "1")
    }

    #[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
    #[must_use]
    pub const fn enabled() -> bool {
        false
    }

    /// `PO_E2E_PAIRING_TTL_SECS=<n>` — how long a pairing payload stays valid.
    ///
    /// Only the side that *creates* a payload reads this: the payload carries its
    /// own `expires_at`, so the expiry test is about the creator's value. The
    /// harness shortens it so a scenario can wait out a real expiry in seconds
    /// rather than the production five minutes; outside a seam build the value is
    /// the constant, so a distributed build cannot be handed a shorter window.
    #[cfg(all(
        any(debug_assertions, feature = "e2e-seams"),
        not(target_arch = "wasm32")
    ))]
    #[must_use]
    pub fn pairing_ttl_secs() -> u64 {
        seam_value("PO_E2E_PAIRING_TTL_SECS")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(super::pairing::TTL_SECS)
    }

    #[cfg(all(any(debug_assertions, feature = "e2e-seams"), target_arch = "wasm32"))]
    #[must_use]
    pub fn pairing_ttl_secs() -> u64 {
        web_sys::window()
            .and_then(|window| window.local_storage().ok().flatten())
            .and_then(|store| store.get_item("po_e2e_pairing_ttl_secs").ok().flatten())
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(super::pairing::TTL_SECS)
    }

    #[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
    #[must_use]
    pub const fn pairing_ttl_secs() -> u64 {
        super::pairing::TTL_SECS
    }

    /// `PO_INSTANCE=<name>` — isolate this process's Veilid data dir and
    /// namespace so two instances can run on one machine (Mac↔Mac tandem).
    #[cfg(any(debug_assertions, feature = "e2e-seams"))]
    #[must_use]
    pub fn instance() -> Option<String> {
        seam_value("PO_INSTANCE")
    }

    #[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
    #[must_use]
    pub const fn instance() -> Option<String> {
        None
    }

    /// `PO_E2E_PAIR_PIN` + `PO_E2E_PAIR_PASSWORD` — what an unattended joiner
    /// fills into the ordinary pairing form (validation and admission are
    /// unchanged; the form's own auto-submit does the rest). Never logged.
    #[cfg(any(debug_assertions, feature = "e2e-seams"))]
    #[must_use]
    pub fn pairing_credentials() -> Option<(String, String)> {
        let pin = seam_value("PO_E2E_PAIR_PIN")?;
        let password = seam_value("PO_E2E_PAIR_PASSWORD")?;
        Some((pin, password))
    }

    #[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
    #[must_use]
    pub const fn pairing_credentials() -> Option<(String, String)> {
        None
    }

    /// `PO_E2E_PAIR_PAYLOAD` — the pairing payload (deep link or raw
    /// base64url) for an unattended joiner. Together with the PIN and
    /// password it fills the whole form, so a device without a typing
    /// channel can still pair. Seam builds only (`e2e-seams` or debug).
    #[cfg(any(debug_assertions, feature = "e2e-seams"))]
    #[must_use]
    pub fn pairing_payload() -> Option<String> {
        seam_value("PO_E2E_PAIR_PAYLOAD")
    }

    #[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
    #[must_use]
    pub const fn pairing_payload() -> Option<String> {
        None
    }

    /// `PO_E2E_ACCOUNT_PIN` — the account's second factor, so an unattended
    /// device can pass the PIN gate that follows admission (login, pairing).
    /// Seam builds only (`e2e-seams` or debug); every other build compiles the
    /// accessor to `None`.
    #[cfg(any(debug_assertions, feature = "e2e-seams"))]
    #[must_use]
    pub fn account_pin() -> Option<String> {
        seam_value("PO_E2E_ACCOUNT_PIN")
    }

    #[cfg(not(any(debug_assertions, feature = "e2e-seams")))]
    #[must_use]
    pub const fn account_pin() -> Option<String> {
        None
    }

    /// Emit one harness marker line, but only in a seam build under the
    /// harness.
    ///
    /// Collapses the `if e2e::enabled() { info!(target: "e2e", event = …) }`
    /// guard that every marker site otherwise repeats. In a build without the
    /// seams the whole call compiles away — the `enabled()` check is a
    /// compile-time `false` — so a shipped binary emits nothing and pays
    /// nothing.
    ///
    /// The fields are forwarded verbatim to `tracing::info!`, so they use that
    /// macro's own syntax (`name = %value`).
    ///
    /// ```ignore
    /// e2e::event!("conv_setup_enter");
    /// e2e::event!("profile_merged", field = %field);
    /// ```
    macro_rules! event {
        ($event:expr $(,)?) => {
            if $crate::config::e2e::enabled() {
                tracing::info!(target: "e2e", event = $event);
            }
        };
        ($event:expr, $($field:tt)+) => {
            if $crate::config::e2e::enabled() {
                tracing::info!(target: "e2e", event = $event, $($field)+);
            }
        };
    }

    pub(crate) use event;
}
