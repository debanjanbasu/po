//! Identity record subkey layout — the frozen wire contract.
//!
//! The identity record is created as `DFLT(16)` and Veilid derives the
//! record key by hashing the owner public key *together with the compiled
//! schema* (`StorageManager::get_record_key`), so the subkey count can never
//! change for existing accounts — a different schema is a different record,
//! stranding every already-paired device. The index assignments below are
//! equally frozen: all app versions address these indices raw off the
//! network. The layout is therefore append-only:
//!
//! - never renumber a slot, and never reuse a retired slot for a different
//!   payload — tombstone it instead;
//! - new identity-level data may claim the lowest spare slot (10–15) **only**
//!   when it must be readable before [`SUBKEY_POINTERS`] during
//!   bootstrap/pairing; everything else belongs in a child record referenced
//!   from [`SyncPointers`](crate::user::sync_models::SyncPointers);
//! - payload evolution happens *inside* a subkey's automerge document — see
//!   the field rules in [`crate::user::sync_models`].
//! - password-encrypted additions must also join the replayable migration in
//!   `user/record/reencrypt.rs`, before publication of the new keypair blob.

/// Subkey count baked into the identity record schema (`DFLT(16)`).
pub const IDENTITY_SUBKEY_COUNT: u16 = 16;

/// [`SyncProfile`](crate::user::sync_models::SyncProfile) automerge doc.
pub const SUBKEY_PROFILE: u32 = 0;
/// [`SyncSecurity`](crate::user::sync_models::SyncSecurity) automerge doc.
pub const SUBKEY_SECURITY: u32 = 1;
/// [`AppState`](crate::models::app_state::AppState) automerge doc — the
/// full model syncs wholesale (no projection).
pub const SUBKEY_APP_STATE: u32 = 2;
/// Encrypted device keypair blob (raw nonce-prefixed bytes, not automerge).
pub const SUBKEY_KEYPAIR: u32 = 3;
/// [`SyncPointers`](crate::user::sync_models::SyncPointers) automerge doc —
/// maps features to child DHT records.
pub const SUBKEY_POINTERS: u32 = 4;
/// Dictionary version metadata (automerge doc).
pub const SUBKEY_DICT_META: u32 = 5;
/// Current compression dictionary blob.
pub const SUBKEY_DICT_BLOB: u32 = 6;
/// Per-device chat private-route entries (length-prefixed binary, plain
/// compressed — see [`crate::chat::signal`] `routes_dht`). Shipped before the
/// layout was centralized; the index is frozen.
// Subkey 7 is retired; do not reuse it.
pub const SUBKEY_ROUTES: u32 = 8;
/// Pairing progress beacon (see [`crate::user::pairing_beacon`]). Ephemeral
/// milestone blob written by a newly paired device and polled by the QR
/// sharing device.
pub const SUBKEY_PAIRING_BEACON: u32 = 9;
// Subkeys 10–15 are unassigned spares. Claim the lowest index and document
// it here; the assignment is permanent once shipped.
