# DHT Record Architecture

**Owns:** `src/veilid/`, `src/user/record/`, `src/user/sync_models.rs`, `src/storage/store_trait/`, `src/chat/record/`

This document describes how po stores and encrypts data on the Veilid Distributed Hash Table (DHT).

## Overview

Every piece of persistent data in po lives either in a **DHT record** (decentralized, cross-device) or in **Veilid TableDB** (local-only). DHT records use `DHTSchema::dflt(N)` — a single-writer schema with N subkeys, each holding up to ~32 KB of independently addressable data.

All DHT store operations obtain a fresh `RoutingContext` from the live Veilid API through the shared helper; the wrapper does not cache contexts or add anonymized safety routing.

## Key Source Files

| File | Purpose |
|------|---------|
| `veilid/store/` | Generic DHT CRUD plus record lifecycle helpers with guaranteed cleanup |
| `user/record/` | Identity record creation, child record resolution, re-encryption |
| `chat/record/` | SMPL conversation record CRUD + conv sync functions |
| `user/sync_models.rs` | CRDT types (`SyncProfile`, `SyncPointers`, `FeatureId`, etc.) |
| `storage/store_trait/` | `SyncSurface` core + `CrdtModel` snapshot subtrait, dual-format local-doc lifecycle, and DHT merge helpers |
| `sync/handler.rs` | Operation dispatch + direct handlers; delegates surface-backed ops to `publish_registered_op` |
| `sync/mutate.rs` | Unified mutation API: `mutate()` (lock→load→closure→save→enqueue) and `load()` |
| `user/identity_context.rs` | `IdentityContext` — bundles key + keypair + secret for identity sync |

---

## Identity Record — `DFLT(16)`

Each user has exactly one identity record with 16 subkeys. The record is created with the user's identity `KeyPair`, so all writes require that keypair.

### Subkey Map

| Subkey | Name | Encryption | Compression | CRDT | Contents |
|--------|------|-----------|-------------|------|----------|
| 0 | Profile | AEAD | zstd (dictionary-free transport) | Automerge | `SyncProfile`: username, DOB, terms_accepted |
| 1 | Security | AEAD | zstd (dictionary-free transport) | Automerge | `SyncSecurity`: PIN hash, biometrics flag |
| 2 | App State | AEAD | zstd (dictionary-free transport) | Automerge | `AppState` (wholesale): nav order, last active route, theme, retention policy |
| 3 | Keypair Blob | None (already password-encrypted) | plain zstd (no dictionary) | None | `nonce_len_LE ‖ nonce ‖ encrypted_keypair` |
| 4 | Registry / Pointers | AEAD | zstd (dictionary-free transport) | Automerge | `SyncPointers`: feature_id → child record map |
| 5 | Dict Meta | AEAD | zstd (dictionary-free transport) | Automerge | Active dictionary content identity (`dict_id` in `DictMeta`) |
| 6 | Dict Blob (Current) | AEAD | plain zstd (no dictionary) | None | Active trained Structured dictionary, validated by content digest |
| 7 | Retired | — | — | — | Retired and intentionally unused; do not assign to a new feature |
| 8 | Route Blob | Unencrypted | plain zstd (no dictionary) | None | Active route entries for `app_message` delivery: `[entry_count: u32 LE][device_id_len: u16 LE][device_id][timestamp_ms: u64 LE][blob_len: u32 LE][route_blob]...` — the bare count stays a raw u32 read; every length-prefixed field is written/read by `helpers::framing` |
| 9 | Pairing Beacon | AEAD | zstd Plain (no domain dict) | None | `PairingBeacon` milestone blob — written by the device pairing against a QR, delivered to the sharing device via its identity-record watch (`user/pairing_beacon.rs`) |
| 10–15 | Reserved | — | — | — | Unassigned spares — claim the lowest index, document it in `identity_layout.rs`; assignments are permanent once shipped |

  - `SyncSurface` impls in `user/sync_models.rs`

### Special Subkeys

- **Subkey 3 (Keypair Blob)**: The keypair is already encrypted with the user's password before storage, under the `po:keypair:v1` AEAD associated-data tag. It goes through `store_with_mode(..., StoreMode::Unencrypted)` which applies only plain zstd compression — no second AEAD layer, since the blob is pre-encrypted.
- **Subkey 5 (Dict Meta)**: `DictMeta` stores the active content identity (`dict_id`, e.g. `b3:<digest>`); new writers publish metadata with the dictionary-free transport frame.
- **Subkey 6 (Dictionary Blob)**: Uses `store_with_mode(..., StoreMode::PlainEncrypted)` / `load_opt(..., false)` — AEAD-encrypted but compressed with plain zstd (no dictionary). This avoids circular dependency (can't use a trained dictionary to compress the dictionary itself).
- **Subkey 7** is retired and unused.

---

## Child Record Registry Pattern

The identity record's 16 subkeys can hold ~512 KB total at ~32 KB per subkey, which is insufficient for features with unbounded data. The **Registry** (subkey 4, `SyncPointers`) solves this by mapping feature identifiers to independent child DHT records.

### How It Works

`SyncPointers` is an Automerge CRDT with:

```rust
pub struct SyncPointers {
    pub pointers: HashMap<FeatureId, ChildPointer>,
}

pub struct ChildPointer {
    pub key: String,       // DHT RecordKey as string
    pub subkey_count: u16, // Expected DFLT(N)
}
```

### FeatureId

`FeatureId` is a string-based newtype — any string is a valid feature ID:

```rust
pub struct FeatureId(pub Arc<str>);
// Core: FeatureId::new("totp"), FeatureId::new("contacts"), FeatureId::new("conversations")
// Plugins: FeatureId::new("com.example.todos")
```

### Resolution Flow

`resolve_child_key_for_id()` in `user/record/` handles lookup and creation:

1. **Check cache** — reads cached `SyncPointers` from the authenticated user in `SESSION`; if absent, reads and hydrates subkey 4.
2. **Check pointer** — a pointer is reused only when its key parses, `subkey_count` matches `child_subkeys`, and its `RecordKey` equals the key deterministically derived from this feature's owner and `DFLT(child_subkeys)`. A malformed key returns an error.
3. **Create or migrate** — otherwise `ensure_child_record()` ensures a record owned by the feature-specific keypair derived from the identity keypair and `child-record:{feature_id}`. A mismatched count or owner/key is replaced with a pointer to this correctly owned record.
4. **Persist migration** — writes changed `SyncPointers` to identity subkey 4 and updates the authenticated user's cached registry.

### SyncSurface / CrdtModel Traits

All synced types implement the `SyncSurface` core trait; dual-format snapshot models add the `CrdtModel` subtrait on top:

```rust
pub trait SyncSurface: Default + Clone + Send + Reconcile + Hydrate + 'static {
    const ID: &'static str;
    const DISPLAY_NAME: &'static str;
    const TABLE_NAME: &'static str = "";
    const SUBKEY: u32 = 0;
    const CHILD_SUBKEYS: u16 = 1;
}

pub trait CrdtModel: SyncSurface {
    const SCHEMA_VERSION: u32;
    fn pack(&self) -> Result<Vec<u8>>;
    fn unpack(bytes: &[u8]) -> Result<(Self, u32)>;
}
```

Identity-doc helpers (`load_doc_bytes`, `store_doc_bytes`, `reconcile_local_doc`, `merge_remote_doc`), dual-format table lifecycle (`load_local`, `save_local`, `replace_local`, `live_doc`, `locked_live_doc`, `merge_remote`, `persist_written_doc`), and both traits live in `storage/store_trait/`. Local helpers (`load_local`, `save_local`, `pull_and_merge`) and `CrdtModel::persist_and_sync` operate on `CrdtModel` directly (`TotpTokenList`, `ContactList`, `ConversationIndex`, and `AppState`), while `SyncProfile` and `SyncSecurity` store their Automerge documents in the shared `po_identity_docs` table.

### Currently Registered Children

| Feature | `FeatureId` | Schema | Contents |
|---------|-------------|--------|----------|
| TOTP | `"totp"` | `DFLT(1)` | TOTP token list |
| Contacts | `"contacts"` | `DFLT(1)` | Contact list |
| Conversations | `"conversations"` | `DFLT(1)` | Conversation index |

Each child record is owned by a deterministic keypair derived per feature from the synced identity keypair, so same-schema features do not share a DHT record owner.

**One root per record.** A child record's local doc must descend from the record's own automerge history. Automerge replaces a whole map object when two **independent roots** are merged, so a local doc built from the snapshot alone (rather than from the network's doc) does not merge key-by-key with the network's copy — one side's whole map wins and the other side's entries are dropped. Every path that publishes a model whose local row does not exist yet therefore seeds the row from the network's doc (`seed_local_doc`), so the local doc shares the record's root and later merges are fast-forwards.

---

## Conversation Records — SMPL Architecture

Conversations use a single `DHTSchema::smpl(3, members)` record per conversation. 1:1 chats are 2-member groups — no special case. The owner gets 3 subkeys (o_cnt=3); each member gets 2 subkeys.

### Subkey Layout

| Subkey | Writer | Contents |
|--------|--------|----------|
| 0 | Owner | `ConversationManifest` (title, owner_identity_key, created_at) |
| 1 | Owner | `ConversationRoster` (members list with role, status per member) |
| 2 | Owner | `ConversationKeyRing` (per-member HPKE-sealed conversation key) |
| 3 + i×2 | Member _i_ | `ParticipantFeedMeta` (`last_seq`, `last_message_at`, `archive_key`, `head_page_start_seq`, `archive_generation`, `archive_root_key`) |
| 3 + i×2 + 1 | Member _i_ | `MessagePage` (messages Vec, page_seq_start/end); each `FeedMessage.msg_id` is exactly `config::chat::MSG_ID_HEX_LEN` (32) lowercase hex chars, enforced on ingest by `normalize_remote_messages`, and cross-device dedup folds the exact id bytes with two fixed-seed `foldhash` lanes (`message_dedup_key`) |

### Design Principles

- **1:1 is a 2-member group** — no special case. Every conversation has a manifest with a roster.
- **Messages live only in participant-owned subkeys** — write authorization is enforced by Veilid's SMPL schema. Per-message content is capped at `config::chat::MAX_MESSAGE_BYTES` (a byte limit — UTF-8 multi-byte characters made a char count bypassable up to 4x) so a single message (with automerge/zstd encoding overhead) always fits inside Veilid's 32768-byte subkey value ceiling.
- **Clients merge all active feeds locally by timestamp** — each client pulls all member subkeys and sorts.
- **One watch per conversation** — SMPL puts everything in one record, so a single `watch_dht_values` covers all changes. Watches are session-owned (armed at login for every indexed conversation, capped at `config::chat::MAX_CONVERSATION_WATCHES`) and cover the full subkey range (`None`), including this member's **own** subkeys — another device on the same account writes the same member subkeys, and the watch is how those sends propagate. Watches are anonymous (`pin_record(key, None)`), so Veilid's per-node `public_watch_limit` (32) applies; each active watch costs roughly 2 RPCs/min to keep alive.
- **Conversation key model** — the owner generates a random `SharedSecret` and HPKE-seals it to each member's identity public key in subkey 2. The keyring's outer envelope is deliberately unencrypted so a new member can bootstrap; the DHT record owner's signature authenticates the keyring, while HPKE base mode is sender-anonymous. Manifest, roster, feed metadata, head pages, and archive pages use the conversation secret.
- **Invite acceptance ordering** — accepting a conversation now hydrates manifest/roster/key-ring plus the current member head pages before any local `ConvFeedSync` enqueue can write feed defaults back to DHT. Archive-chain history backfills in a session-scoped background task so accept is not O(history), while full hydration still walks `load_archive_chain()` and now surfaces corrupt archive indexes as explicit errors instead of silently defaulting them away.

---

## Encryption Pipeline

### Write Path

```
raw bytes
  → zstd compress (`ZSTD_LEVEL` = 12) with the stable version-zero dictionary-free transport framing
  → AEAD encrypt (random nonce + SharedSecret)
  → prepend nonce bytes
  → set_dht_value()
```

Both compression and encryption run in a single `spawn_blocking` task via `compress_and_encrypt()` to minimize context switches.

### Read Path

```
get_dht_value()
  → extract nonce prefix
  → AEAD decrypt with SharedSecret
  → read the frame header (version-zero transport or content-identified local)
  → zstd decompress — dictionary-free for transport frames; content frames use their named learned dictionary
  → return raw bytes
```

Schema versions, where applicable, are encoded inside the Cap'n Proto model bytes rather than by the encryption layer.

All snapshot schema constants start at `USER_SCHEMA_VERSION = 1`; each Cap'n Proto
snapshot model carries its own schema version in the model bytes, and strict reads reject a
mismatch. Message-status and `PeerSignal` tags are unchanged.

### SharedSecret Derivation

The `SharedSecret` used for AEAD is derived from the user's password:

```
password → Argon2id credential hash (salted with the 16-byte account ID)
         → material = "po:dht-secret:v1\0" ‖ 16-byte account ID ‖ credential hash
         → derive_shared_secret(material, DHT_SECRET_DOMAIN_KEY) // constant domain key
         → SharedSecret
```

The 16-byte random account ID is stored as 32 lowercase hexadecimal characters and decoded
before derivation. `DHT_SECRET_DOMAIN_KEY`
(`src/helpers/encryption.rs`, currently
`b"po:dht-secret:v1:domain-key:0123456789abcdef"`) is a compile-time constant domain
separator, not a secret: all entropy lives in the Argon2 credential hash, so every build
of a revision derives the same account keys and there is no build-time secret to
provision, plumb through CI, or leak from an artifact.

A login hashes the password once (`src/user/auth/login_signup.rs::login_with_password`) and
uses the same `CURRENT_CRYPTO_VERSION = 1` storage layout as every other build.

Pairing payloads (version `PAIRING_VERSION = 1`) are framed as
`account_id(16) ‖ nonce(24) ‖ ciphertext`, then encoded as unpadded base64url. The account
ID travels in the clear — it is public by design — because the joiner needs it to reproduce the
credential hash that keys the payload before it can decrypt anything. The encrypted body is
`version(1) ‖ expiry(8, u64 LE) ‖ crypto kind(4) ‖ DHT record key(65) ‖ keypair(64) ‖
account ID(16)`: the version byte names this layout from inside the ciphertext, the record
key is `kind(4) ‖ opaque key(32) ‖ encryption-key-present(1) ‖ optional encryption key(32)`,
and the body repeats the 16-byte account ID after the keypair. The payload key is
`derive_shared_secret("po:pair:v1:" ‖ credential hash ‖ PIN, PAIRING_DOMAIN_KEY)`, under the
constant `b"po:pair:v1:domain-key:0123456789abcdef"` (`src/user/pairing_crypto.rs`); its
entropy is the credential hash plus the one-time 12-digit PIN.

### HPKE Key Wrapping

Conversation keys are wrapped per member using HPKE RFC 9180 base mode in `src/chat/conv_keys.rs`, through the cached crypto system in `src/helpers/encryption.rs`. The owner generates a random `SharedSecret`; `wrap_conversation_key()` derives an encapsulation key from each member's identity public key and seals the key bytes with the `po.conversation-key.hpke.v1` AAD. `unwrap_conversation_key()` opens it with the member's identity secret. HPKE base mode does not authenticate a sender; the DHT record owner's signature authenticates the key ring.

### DHT Transactions

Writes that must be atomic across multiple records use `veilid::store::with_transaction`. Transactions stage writes locally and commit atomically with rollback on supersede. TableDB also supports atomic multi-key writes via transactional batches. This underpins the staged post-admission commit during pairing and prevents partial updates during Automerge merges. Dictionary publication (`dict_sync::publish_to_dht`) runs as one transaction: existing metadata is read inside the transaction, then subkey 5 (`dict_id` metadata) and subkey 6 (active content blob) are written atomically — a superseded or oversized publication leaves the previous announcement and blob intact.

### Store Variants

| Function | Compression | Encryption | Use Case |
|----------|-------------|------------|----------|
| `store()` | zstd (dictionary-free transport frame) | AEAD | Default — profile, security, app state, pointers, chat data |
| `store_with_mode(..., StoreMode::Unencrypted)` | plain zstd (no dictionary) | None | Pre-encrypted data (keypair blob) and public route blobs |
| `store_with_mode(..., StoreMode::PlainEncrypted)` | plain zstd (no dictionary) | AEAD | Dictionary blobs and pairing beacons (no dictionary dependency) |
| `load_opt(..., true)` | decompress (frame-header selected) | AEAD decrypt | Default structured read |
| `load_opt(..., false)` | decompress (plain zstd, no dictionary) | AEAD decrypt | Dictionary blob and pairing beacon read |
---

## Record Lifecycle

`store::with_record()` provides RAII-style record management:

```rust
pub async fn with_record<F, Fut, T>(
    key: RecordKey,
    keypair: Option<KeyPair>,
    f: F,
) -> Result<T>
```

1. Opens the DHT record (`open_record` with optional writer keypair).
2. Runs the async closure `f` with a 30-second timeout.
3. **Always** closes the record — even if `f` errors or times out.
4. Returns the closure's result or a timeout error.

A `with_record_timeout()` variant accepts a custom `Duration`. Convenience readers wrap the open → read → close pattern for single reads: `read_subkey()` decodes normally, `read_subkey_raw(key, subkey, force_refresh)` returns raw bytes (migration preflight passes `force_refresh = true` so remotely-published children are not missed), and `read_subkey_raw(key, subkey, false)` is the cache-preferred variant. Signing owner keys come from `store::record_owner(key)` — the open record's Veilid descriptor (writer pin + record scope), never record-key digest bytes; same-key nested lifecycle calls are prohibited.

### Why This Matters

Veilid requires explicit `close_dht_record` calls. Leaked open records consume local resources and can block subsequent opens. `with_record` guarantees cleanup.

### Cold-Routing-Table Opens (`TryAgain`)

Veilid reports an open that reached no node as `VeilidAPIError::TryAgain` — "offline, try again later" when the DHT is offline, or "no node answered the open" when the fanout found nobody (veilid-core `storage_manager/open_record.rs`). Both are explicitly retryable, and neither means the record is missing.

`open_record()` therefore retries `TryAgain` with a short backoff (6 attempts, 750 ms apart) before surfacing it as a hard error; every other variant stays terminal. Without this, a device that paired seconds after attaching failed its first attempt — its routing table was still cold, so the fanout reached nobody — even though the network became ready moments later. This hit the identity pull and the child-record pin, and only the E2E harness's retry seam (release builds have none) hid it from real users.

**The readiness gate has to match Veilid's, and did not.** Retrying treats the symptom; the cause was that the app's readiness predicate disagreed with the gate Veilid checks before it will open anything. The storage manager gates every record open on its own `dht_is_online` flag, set from the **PublicInternet** routing domain becoming ready in both directions — the same level-based signal the attachment state publishes as `public_internet_ready` (veilid-core `storage_manager/mod.rs` says so outright). The app waited on `NetworkStatus::has_connectivity` (attached + one live peer) instead, which is strictly weaker: a node that has just attached loads its routing table from disk, so it reports live peers while the storage manager still refuses the open and sends no packet at all. `NetworkStatus::dht_ready()` is that gate, and `wait_for_dht_readiness` waits on it.

LAN does not substitute for it: `routing_domain_ready_event_handler` returns early for every routing domain except PublicInternet, so **LocalNetwork readiness alone cannot make an open succeed**. `local_network_ready` is a peer-reachability signal, not a DHT one. The same reasoning makes `has_connectivity` the right predicate for transport-level work and the wrong one for anything that opens or reads a record.

A `TryAgain` that survives the retries now reports as `NewDeviceLoginError::NetworkNotReady` — "This device couldn't connect to the sync network in time…" — rather than as an internal error, because the pairing data was never the problem.

Note that `ensure_record_with_keypair()` already treats `KeyNotFound | TryAgain` as "create locally"; `open_record()` opens only existing records, so it must not create them.

### Cold-Routing-Table Subkey Reads (`TryAgain`)

The record open was not the only cold-table failure. **`get_dht_value` has the same
gate**: it blocks on a network fanout and returns `TryAgain` when the node is
offline (veilid-core `storage_manager/open_record.rs` documents the same
"offline, try again later" contract for the value read). The open had a retry;
the value read did not.

That gap cost a real device its first pairing. The iPhone's post-pairing
identity pull ran ~3 s after launch — while the routing table was still cold —
and the subkey read died on its first `TryAgain`. `prepare_pull()` returns
`None` on that failure, so `post_pairing_hydration()` cleared the pairing
status and returned **before** reaching the `event="paired"` marker: the app
sat on the home screen looking paired, but the sync never ran and the user saw
no completion. The network came up 0.6 s later.

`get_dht_value_with_retry()` (in `src/veilid/store/api.rs`) now applies the
same budget as the record open — 6 attempts, 750 ms apart — and both subkey
read paths use it: the decoded read (`load_opt_with_mode`) and the raw read
(`read_subkey_raw`, used by the migration preflight). Every other error variant
stays terminal.

The deeper fix belongs to the readiness gate above: the pairing path should
not start a network pull before the DHT is online. The retry makes the failure
survivable; the gate makes it unnecessary.

### Password Rotation Re-Encryption

Password rotation (`src/user/record/reencrypt.rs`) re-encrypts stored identity values under the new password hash using **raw transaction reads/writes** — compressed bytes are read and written verbatim without needing their dictionary, because the DHT transport frame is dictionary-independent. Re-encryption runs inside the record's `with_record_timeout` scope; the rotation journal lives in the `po_users` table's third column, and the secret history/migration policy lives in `user/record/reencrypt.rs`. Migration preflight (`child_has_values`) reads with `read_subkey_raw(..., true)` before starting a transaction so remotely-published children are detected; `read_subkey_raw(key, subkey, false)` is the cache-preferred variant.

---

## Sync Pipeline Integration

All mutations flow through the unified sync pipeline:

```
save locally → enqueue PendingOperation → background drain loop → IdentityContext dispatch → DHT write
```

The drain loop in `sync/queue/` constructs an `IdentityContext` once per operation, which bundles:

```rust
pub struct IdentityContext {
    pub user: Arc<User>,
    pub key: RecordKey,
    pub keypair: KeyPair,
    pub secret: SharedSecret,
}
```

`handler::dispatch()` in `sync/handler.rs` handles `CreateIdentity` and the operation-specific flows, then sends surface-backed operations through the explicit static `publish_registered_op()` match in `user/sync_ops.rs`. This is not runtime startup registration, and the model macro does not populate it.

Core surface and sync-operation APIs:

| Method | OpType / surface | What It Does |
|--------|------------------|-------------|
| `IdentityContext::sync_subkey::<T>()` | Identity subkey surfaces | Loads local `T`, reconciles with DHT, writes back |
| `IdentityContext::sync_child_crdt::<T>()` | Child-record surfaces | Resolves the feature pointer, loads local `T`, reconcile-writes with its feature-derived owner |
| `publish_registered_op()` | `Profile`, `Security`, `AppState`, `Totp`, `Chat`, `ChatContacts` | Explicit static map from surface-backed ops to the identity/child publish path |
| `sync_ops::sync_profile_and_security()` | `ProfileAndSecurity` | Composite handler dispatched directly by `sync/handler.rs` |
| `sync_ops::reencrypt_subkeys()` | `ReencryptSubkeys` | Password-rotation handler dispatched directly by `sync/handler.rs` |
| `dict_sync::publish_to_dht()` | `DictPublish` | Reads metadata inside one transaction and writes subkeys 5/6 atomically |

The conversation feed sync function `sync_conv_feed` is a standalone function in `chat/record/` that takes `(&PendingOperation, &IdentityContext)`; manifest, roster, and key-ring subkeys are written directly by their feature flows (bootstrap, invite processing, member updates) rather than through dedicated queue ops. Sync reconciles the original local CRDT history before merging remote values and persists the merged returned value — no fresh-root DHT compaction without a coordinated generation protocol.

---

## Adding a New Feature with DHT Storage

### Step 1: Define the Model

Create a struct with `Reconcile` + `Hydrate` derives (for Automerge CRDT support):

```rust
#[derive(Debug, Clone, Default, Reconcile, Hydrate)]
pub struct MyFeatureData {
    pub field_a: String,
    pub field_b: u64,
}
```

### Step 2: Register the Model

Use the `impl_crdt_model!` macro, which emits both the `SyncSurface` core impl
and the `CrdtModel` snapshot codec. Define the packed Cap'n Proto encoder
(`&T -> Result<Vec<u8>>`) and version-reporting decoder
(`&[u8] -> Result<(T, u32)>`) first. The existing invocation in
`src/models/totp.rs` is the reference shape:

```rust
impl_crdt_model!(
    TotpTokenList,
    id = "totp",
    display = "TotpTokenList",
    op = OpType::Totp,
    schema = TOTP_SCHEMA_VERSION,
    pack = pack_totp_list,
    unpack = unpack_totp_list,
);
```

`CrdtModel`, `OpType`, and `impl_crdt_model` must be in scope. Adapt the model, ID, operation, schema constant, and codec names for the new feature. Identity-subkey `CrdtModel`s (such as `AppState`) append `subkey = N`.

### Step 3: Add the Dispatch Arm

A new operation needs both the exhaustive dispatch arm in `sync/handler.rs`
and a publishing arm in `user/sync_ops.rs::publish_registered_op`. The latter
is the static registry; the macro does not populate it automatically.
Extend the corresponding child hydration/watch dispatch when adding a new
child feature, not just its outgoing publish path.


### Step 4: Use `mutate()` for Mutations

Use the unified mutation API from `sync/mutate.rs`:

```rust
let ctx = UserCtx::try_current().context("not logged in")?;
mutate::<MyFeatureData, _>(&ctx, |data| {
    data.field_a = "updated".into();
    Ok(MutationResult::modified(()))
})
.await?;
```

`mutate()` handles: lock → load local → apply closure → save local → enqueue `PendingOperation`. The drain loop dispatches it through the exhaustive `OpType` match in `handler::dispatch()`.

### Step 5: Update Documentation

Update the child record table in this file and the dispatch table in [docs/sync-pipeline.md](sync-pipeline.md).
