# Sync Pipeline Architecture

**Owns:** `src/sync/`, `src/sync/pending_ops/`, `src/sync/queue/`, `src/chat/record/`

## Overview

The sync pipeline implements **offline-first synchronization**: all mutations save locally first, then enqueue a `PendingOperation` for background DHT sync. No code path writes directly to DHT — every mutation flows through the queue.

```
local save → enqueue (TableDB) → notify → drain loop → IdentityContext / chat_record dispatch → DHT write
```

## Local-first boundary

- **Durable state** (user/profile/security/app state, contacts, conversations, TOTP, dictionaries) must follow the queue pipeline above.
- **Ephemeral control-plane signals** (typing/read/new-data nudges) may use direct peer signaling and are explicitly non-durable accelerators.
- If a flow needs durable remote convergence, it must enqueue an operation even when a fast-path network call exists.

## Key Files

| File | Purpose |
|------|---------|
| `src/sync/pending_ops/mod.rs` | Queue facade (public API), dedup indexes, dead-letter orchestration |
| `src/sync/pending_ops/indexes.rs` | In-memory indexes (`pending`, `pending_by_id`, `dead_letter`, dedup maps) |
| `src/sync/pending_ops/storage.rs` | TableDB persistence/load helpers + stale rewrite hooks |
| `src/sync/queue/` | Background drain orchestration (network gate, wake scheduling, dispatch loop) |
| `src/sync/queue_policy.rs` | Queue policy: `OpFamily` enum, `LaneKey`, ready window, active-user prioritization, retry/dead-letter decisions |
| `src/user/identity_context.rs` | `IdentityContext` — bundles record key, keypair, secret, user for sync |
| `src/user/identity_layout.rs` | Single source of truth for identity record subkey assignments |
| `src/storage/store_trait/` | `CrdtModel` trait + dual-format storage (`load_local`, `save_local`, `persist_and_sync`, `pull_and_merge`) with auto-compaction |
| `src/storage/snapshots/` | Snapshot-history indexes, CRDT compaction, retention metadata |
| `src/storage/mod.rs` (`sync_status`, `pairing_sync_status`, `training_status`, `upgrade_status`, `hydration_status`) | Operational status signals (`SYNC_STATUS`, `PENDING_OPS_REVISION`, `PAIRING_SYNC_STATUS`, `TRAINING_STATUS`, `UPGRADE_STATUS`, `HYDRATION_STATUS`) |
| `src/storage/automerge.rs` | Core automerge ops: `reconcile_to_automerge`, `from_automerge`, `merge_docs`, `merge_and_hydrate_with_stale_check` |
| `src/sync/handler.rs` | Operation dispatch + direct composite/system/conversation handlers |
| `src/user/sync_ops.rs` | Identity sync handlers and the static `publish_registered_op` surface map |
| `src/sync/mutate.rs` | Unified mutation API: `mutate()` |
| `src/components/overlays/activity_bar.rs` | Compact network/sync/training/recompression status indicator |
| `src/components/overlays/activity_modal.rs` | Queue + dead-letter + background task detail surface |
| `src/veilid/store/` | `with_record` / `with_record_timeout`, `WriteOutcome`, `reconcile_write` — DHT lifecycle + write-conflict semantics |
| `src/user/record/` | Identity record CRUD, child record resolution, and pointer reconciliation |
| `src/user/identity_watch.rs` | Session-pinned identity record + whole-record DHT watch |
| `src/user/pairing_beacon.rs` | Coarse pairing-progress beacon (identity subkey 9) |
| `src/chat/record/mod.rs` | Conversation record facade + shared subkey helpers/constants |
| `src/chat/record/bootstrap.rs` | Local conversation build + DHT bootstrap (`ConvRecordCreate`) |
| `src/chat/record/feed_sync/` | Feed/meta sync + archive rotation + owner-subkey sync + shared sync preamble/context |
| `src/chat/record/hydration.rs` | Conversation hydration + head/archive orchestration |

## Flow in Detail

### 1. Local Save

A model calls `persist_and_sync()` from `storage/store_trait/`. This:

1. Writes the Cap'n Proto snapshot (column 0); the Automerge sidecar is reconciled from it on-demand before sync.
2. The sync reconciliation checks the Automerge document's raw (uncompressed) length against the compaction threshold and may schedule compaction (see [Dual-Format Storage & Auto-Compaction](#dual-format-storage--auto-compaction)).
3. Creates a `PendingOperation` with a monotonic **queue sequence number** — this is durable queue identity for ordering/dedup/ownership only. It is **not** a DHT sequence number; Veilid owns per-subkey DHT sequence numbers entirely (see [DHT Write Semantics](#dht-write-semantics-writeoutcome)).
4. Calls `enqueue()` to persist the operation.

Steps 1–2 (save) and 3–4 (enqueue) execute **sequentially** — save completes before enqueue begins.

### 2. Enqueue

`pending_ops::enqueue()` persists the op to TableDB and inserts it into an in-memory `BTreeSet` index sorted by `(scheduled_at, sequence_number, id)` for ordered processing.

**Squashing**: Every enqueue keeps its freshly assigned `id` and `sequence_number`. The dedup key `(account_id, OpType, target)` atomically hands ownership to the newest pending row, so rapid local edits still collapse to one latest-write sync without reusing queue identity. (`Chat` squashes `ConversationIndex` syncs, while `ConvFeedSync` unions queued message IDs across coalesced feed publishes.) `target` is typed, not a raw string: most ops carry one key, and an invite retry carries both the recipient peer and the conversation it opens (`DedupTarget::Invite`), so two conversations with the same peer stay distinct without joining keys into a delimited string.
On collision, the previous owner is removed from the in-memory `BTreeSet` / `pending_by_id` / lane-tagged dedup indexes and is best-effort deleted from TableDB. The dedup lock key length-prefixes each target string through `helpers::framing` (`push_str`), so no two keys serialize to the same bytes. If an older row survives a crash or partial transition, `init_indexes()` self-heals by keeping the newest pending-or-DLQ owner for that dedup key and pruning stale duplicates.

Queue loads reject rows whose decoded operation ID does not match the TableDB key. Runtime corrupt-row cleanup compares the observed value again under the shared row lock before deleting or removing its index; newer-schema rows remain preserved.

### 3. Notify

`sync_queue::notify_sync()` wakes the background processor via `tokio::sync::Notify`.

### 4. Drain Loop

`sync_queue::start_background_processor` runs a **lane-based parallel** drain loop:

1. Waits on the `Notify` signal.
2. Dispatches ready work regardless of app-level connectivity; Veilid owns offline buffering, while unreachable attempts do not consume the app retry budget.
3. Reads pending ops, groups them by `LaneKey` (derived from `OpFamily` + target), and dispatches every lane at once through the unbounded `helpers::concurrency::map_concurrent` fan-out (see docs/concurrency.md).
4. Just before `handler::dispatch()`, each op re-checks currentness/version (`id` + `sequence_number` ownership). Replaced rows short-circuit before any stale DHT side effects can run.
5. **Per-lane ordering is preserved** (ops within a lane execute sequentially). **Cross-lane execution is concurrent** (e.g., profile sync and conversation sync run in parallel).
6. Uses a `recheck` flag — if new work arrives during processing, another drain cycle runs immediately after.

Lane assignment uses `OpFamily` (from `queue_policy.rs`): `Identity`, `Conversation`, `System`, etc. Each family + target key produces a unique `LaneKey`.

A `ProcessingGuard` ensures the `IS_PROCESSING` flag resets even on panic.

Veilid's count-based operation cap is always active. Its adaptive upload/download
byte budgets are optional: `aimd-dht` enables the upstream experimental AIMD
controller, and `veilid-stats` emits counters for controlled comparisons.

### 5. Handler Context Construction

The drain loop calls `handler::dispatch(op, lane_identity)`. Dispatch first handles `CreateIdentity`, which needs no identity context. Every other `OpType` reuses the lane's pre-resolved `IdentityContext` or resolves one with `IdentityContext::for_user(&op.username)`.

`IdentityContext::for_user()` resolves:

- User's DHT record key
- Keypair (from session crypto, in-memory `User::runtime.keypair`, or the stored encrypted keypair blob — last fallback requires "remember me" to be active on this device so the password hash is persisted locally)
- Encryption secret (derived from password)
- Username

Identity operations call methods on that context. Conversation sync functions are standalone in `chat/record/` and receive the context as a parameter.

### 6. Operation Dispatch

`dispatch()` in `sync/handler.rs` handles `CreateIdentity` and explicit composite/system/conversation operations. Other surface-backed ops go through the static `publish_registered_op()` match in `user/sync_ops.rs`; it is a manually maintained map, not runtime registration or macro-generated dispatch.

| OpType | Handler | Notes |
|--------|---------|-------|
| `AppState` | `sync_ops::publish_registered_op()` → `sync_app_state(id)` | Registered `CrdtModel`; reconciles `po_app_state` to identity subkey 2 |
| `CreateIdentity` | `sync_ops::create_identity(op)` | Free function in `user/sync_ops.rs`; needs no identity context |
| `Profile` | `publish_registered_op()` → `id.sync_subkey::<SyncProfile>()` | Automerge reconcile with DHT |
| `Security` | `publish_registered_op()` → `id.sync_subkey::<SyncSecurity>()` | Automerge reconcile with DHT |
| `ProfileAndSecurity` | `sync_ops::sync_profile_and_security(id)` | Composite handler; enqueued at signup to publish onboarding state |
| `Totp` | `publish_registered_op()` → `id.sync_child_crdt::<TotpTokenList>()` | Child record via `CrdtModel` |
| `Chat` | `publish_registered_op()` → `id.sync_child_crdt::<ConversationIndex>()` | Child record via `CrdtModel` |
| `ChatContacts` | `publish_registered_op()` → `id.sync_child_crdt::<ContactList>()` | Child record via `CrdtModel` |
| `ReencryptSubkeys` | `sync_ops::reencrypt_subkeys(op, id)` | Free function in `user/sync_ops.rs`; password-change migration |
| `DictPublish` | `dict_sync::publish_to_dht(&id.key, &id.keypair, &id.secret)` | Compression dictionaries |
| `ConvFeedSync` | `chat_record::sync_conv_feed(op, id)` | Writes head page + feed meta to SMPL record and handles archive rotation |
| `ConvFeedSync` (archive chain depth cap, 64 generations) | — | Write side: at the per-epoch cap the chain ROLLS OVER — `archive_epoch` increments (monotonic counter derived from chain state, so racing devices derive the identical next epoch), `archive_generation` resets to 0, and tail/root keys clear so the next page starts a fresh chain (fresh derived owners scoped by epoch). The prior chain's root stays on the DHT; pre-rollover history remains retrievable from every device's local message store — archive chains only serve cold hydration. Load side: `load_archive_chain` walks a single chain and returns partial history past the cap with a nonfatal report. Local storage: `FEED_META_SCHEMA_VERSION` is 2 (`archiveEpoch @7`); pre-epoch automerge feed-meta docs hydrate with `archive_epoch = 0`, and a stored submodel decoding as schema-Older triggers the same replace-on-Older path as an older envelope. |
| `ConvInviteSignal` | `conv_invite_signal(op, id)` | Retries invite delivery |
| `ConvRecordCreate` | `conv_record_create(op, id)` | Creates the conversation SMPL record on DHT |

`SyncPointers` is not a queued `OpType`: `resolve_child_key_for_id()` persists the registry directly when a feature pointer is created or migrated.

All DHT writes go through `store::with_record`, which opens/closes the DHT record around the operation with a 30-second timeout and guarantees close cleanup.

### 7. Retry

Failed ops (via `queue_policy::retry_decision`):

1. Increment `attempts` counter.
2. Get exponential backoff via `queue_policy::retry_params(op_type)`:
    - **Default**: `2^attempts × 1s`, capped at 1 hour, 5 max retries.
    - **`ReencryptSubkeys`**: **indefinite retries** (`max_retries == 0`) using the default backoff, so password rotation resumes even if attempts run while logged out.
    - **`ConvInviteSignal`**: `2^attempts × 5s`, capped at 2 hours, `INVITE_MAX_RETRIES` (12) max retries. Invite delivery only lands when the peer is online, so the budget is attempts, not wall time (~2 days at max backoff); delivered-but-unacked invites (`InviteAwaitingAck`) don't consume budget. The early attempts stay close together (10s, 20s, 40s, 80s) so a peer that is momentarily unreachable — a network flap, a route that has not settled yet — gets the invite within the same interaction. Beyond the budget the invite dead-letters, keeping a dead contact from squatting a retry lane forever — the DLQ retry path gives manual recovery.
3. `scheduled_at` is set to the future retry time.
4. The queue's next wake time accounts for the earliest scheduled op — no busy-waiting.

### 8. Dead Letter Queue (DLQ)

Finite-retry ops move to the DLQ after `MAX_RETRIES` (5) failures; `ReencryptSubkeys` retries indefinitely, and `ConvInviteSignal` dead-letters after `INVITE_MAX_RETRIES` (12) failures. Under the dedup-key lock, `move_to_dead_letter()` calls `store_op_replacing()` to atomically write the DLQ row and retire its pending predecessor before updating in-memory indexes. There is no separate `delete_pending` leg. The APIs are `retry_from_dead_letter()`, `retry_all_dead_letters()`, `dismiss_from_dead_letter()`, and `dismiss_all_dead_letters()`.

Conversation feed messages persist a local delivery status (`Pending`, `Sent`, or `Failed`). Each `ConvFeedSync` payload contains the exact set of message IDs it covers as a sorted, newline-delimited list. Squashing unions the displaced and replacement operations' ID sets. Success and DLQ transitions therefore update only the covered messages; an empty set is valid for feed syncs that cover no delivery rows. The conversation UI exposes failed-message retry, which returns exactly the selected message to pending before re-enqueuing the conversation feed.

Dead-letter retry refuses to overwrite a newer pending owner for the same dedup key. In that case the stale DLQ row is discarded (and any displaced same-id pending row is cleaned up) instead of resurrecting older state over the replacement. If the stale row cannot be deleted immediately, the queue persists a row-cleanup marker in that column so restart rebuild drops the residue instead of reviving it. Likewise, successful completion deletes lingering same-id pending + dead-letter residue together, and falls back to a durable completion marker if final cleanup cannot finish atomically.

### Queue/Training/Upgrade Status Signals

Operational status is exposed to `ActivityBar` and `ActivityModal` by phase
signals plus a monotonic queue-content revision:

- `SYNC_STATUS` (`storage::sync_status`) tracks queue phase + pending/dead-letter counts.
- `PENDING_OPS_REVISION` (also `storage::sync_status`) is bumped by
  `refresh_sync_status()` on every queue mutation, including a same-count
  replacement. Readers that render **row contents** subscribe to it in addition
  to `SYNC_STATUS`; a UI keyed only on the phase can miss a squash or re-enqueue.
- `TRAINING_STATUS` (`storage::training_status`) tracks dictionary training lifecycle.
- `UPGRADE_STATUS` (`storage::upgrade_status`) tracks owner-scoped local recompression and DHT dictionary-sync progress.

`SYNC_STATUS` refresh is now intentionally **coalesced** for drain-owned mutations: `process_queue()` refreshes once when a drain starts, `ProcessingGuard::drop()` refreshes once when it finishes, and per-op bookkeeping inside `complete()` / `move_to_dead_letter()` no longer rewrites the signal after every row transition. Direct/manual queue mutations that are not followed by the guard (`enqueue`, single/bulk DLQ retry, single/bulk DLQ dismiss, startup rebuild, account cleanup) still refresh immediately.

`SYNC_STATUS` is device-wide: queue counts remain visible across logout, and session teardown does not clear the signal.

### Pairing Flow

Pairing hydrates a new device from DHT into local storage; it is not a background push of local state. The stages are `Connecting` → `Identity` (fetch identity subkeys), optionally `WaitingForSource` (re-poll until onboarding data appears), then `Admission`. After the matching authenticated admission, `AppState` stores app settings and staged identity docs locally; detached post-admission hydration then runs `ChildRecords`, `Conversations`, and `Dictionary`, ending at `Complete`. Failures after admission are non-fatal and surface through `PAIRING_SYNC_STATUS`.

## DHT Write Semantics (`WriteOutcome`)

**Veilid owns DHT sequence numbers.** The app never tracks, stores, or compares per-subkey DHT sequence numbers. Write conflicts surface through Veilid's native `set_dht_value` return value, wrapped in `WriteOutcome` (`src/veilid/store/`):

| `set_dht_value` returns | `WriteOutcome` | Meaning |
|--------------------------|----------------|---------|
| `None` | `Written` | Accepted **locally** — possibly offline-deferred, not network-confirmed. Veilid queues offline writes in its own table store and replays them on reattach/restart. |
| `Some(newer)` | `Superseded(Bytes)` | The network already holds a **newer** value for this subkey; the write was **not applied**. The payload is that newer value, decoded (decrypt + decompress) through the same mode as the attempted write. |

Callers pick one of three policies:

- **CRDT merge-and-retry** (`reconcile_write` / `reconcile_write_open`): the first attempt merges local state with any network history; on `Superseded`, the newer doc becomes the base and the attempt is rebuilt as `reconcile(newer_base, value)` (bounded by `RECONCILE_CONFLICT_ATTEMPTS`), so each retry starts from a single root and can never produce a double-root doc. Used for all Automerge subkeys.
- **Absolute writes** (`require_written()`): losing to a concurrent writer is a retryable failure — the op fails and the sync queue re-runs it against fresh state. Used for record bootstrap, key rings, keypair blobs, dictionary publishes, route republishes.
- **Newest-wins tolerance**: supersession is benign where a newer network value already satisfies the write's purpose — e.g. the pairing beacon (`user/pairing_beacon.rs`), where a newer milestone landing first is success, not failure.

The queue remains necessary even though Veilid handles offline write replay: CRDT read-merge-write cycles, record creation, multi-step workflows (archive rotation, re-encryption), non-DHT effects (peer signals), and any case where a record cannot be opened offline all need durable app-level retry.

## Sync Methods

### `sync_subkey<T>`

1. Opens DHT record via `with_record`.
2. Loads existing value from DHT subkey.
3. Loads local value from TableDB.
4. Reconciles via automerge (CRDT merge).
5. Writes merged result back to DHT.

### `sync_child_crdt<T>`

1. Resolves child record key via `resolve_child_key_for_id()` (looks up Registry at subkey 4 using `SyncSurface::ID` and `T::CHILD_SUBKEYS`).
2. Loads the local live `CrdtModel` document with its Automerge history via `locked_live_doc::<T>()`.
3. Calls `reconcile_write` on the child record (loads existing DHT doc, reconciles local state into it, writes back).
4. On schema validation failure (stale/corrupt record), clears cached pointer so the next attempt re-resolves.

> **Note:** `pull_and_merge()` loads the remote automerge doc from DHT first, then holds the captured username lock and verifies the account before merging both formats locally (Cap'n Proto snapshot + automerge doc).

### Read/write freshness (`force_refresh`)

Veilid persists a local copy of every record it has seen, and a non-forced `get_dht_value` returns that copy when present — across app restarts. Freshness is therefore explicit on both directions of the pipeline:

- **Publishes** (`publish_identity_subkey`, `sync_child_crdt`) call `reconcile_write` with a **forced** base read: reconciling against the device's own cached copy would build the merge on a stale base and clobber another device's newer value.
- **Resync pulls** are forced too: `pull_and_merge` force-reads the child subkey, and identity pulls (`src/user/record/::sync_with_policy`) run one `SyncGet` inspect first, force-reading only the subkeys the network holds newer (`newer_online_subkeys()`); an inspect failure forces everything.
- **Cold reads** (create-once pointers, roster on first open) stay non-forced — the local copy is absent or authoritative, and a missing local value falls through to the network anyway.

### Conversation Sync Functions (`chat/record/feed_sync/`)

Standalone functions that take `(op, &ctx)` and handle conversation-specific DHT writes:

- **`sync_conv_feed` and archives**: Before building the head page, it force-reads this member's DHT feed metadata and page, merges unseen remote messages into the local conversation with `union_messages_in_place`, and retains those remote Automerge documents as reconcile bases. Overflow pages are written to deterministic per-member, per-conversation, per-generation archive records; each immutable page and its `ArchiveIndex` update commit in one DHT transaction. Retries reuse the derived archive record, skip already-written pages, and follow the indexed `next_archive_key` chain when a generation fills.
  - The SMPL head page and feed metadata are reconcile-written against the observed Automerge histories. Superseded writes re-reconcile `value` onto the newer DHT document and retry (fresh root per attempt — no double-root docs); the returned values are then unioned/persisted locally, so concurrent-device messages are not treated as last-writer-wins replacements. Without rotation the observed histories are kept as bases, preserving concurrent local edits.
  - At archive rotation (`head_page_start_seq` advanced — either the remote meta shows another device rotated, or this publish rotates itself), the head page and feed metadata are written from a **fresh Automerge root** (`local_doc = None`, reconciled against the network base). This bounds the head/meta docs to roughly one rotation cycle instead of growing by one cycle per rotation until they exceed Veilid's 32768-byte subkey ceiling; message content lives in archive pages + local store and is reconstructed by `union_messages_in_place` (merges are keyed on msg_id, so fresh roots lose no data).

## Dual-Format Storage & Auto-Compaction

### Dual-Format Storage

`CrdtModel` types use a two-column TableDB layout for each model table (`po_{model_id}`):

| Column | Format | Purpose |
|--------|--------|---------|
| Col 0 | Cap'n Proto packed bytes | **Fast reads** — zero-copy deserialization for UI rendering |
| Col 1 | Automerge document bytes, DEFLATE (`save()`) | **CRDT merge** — full change history for conflict-free sync |

**Doc form**: every producer in `storage/automerge.rs` emits `save()` — automerge's own DEFLATE form — and zstd (`ZSTD_LEVEL` = 12) compresses it like any other row; there is no second stored form (see [compression.md](compression.md) for the measurements). `AutoCommit::load` reads either that form or the older uncompressed one, so pre-existing rows keep loading; a row still holding the old form is rewritten the next time it is reconciled or merged.

**Read path**: `load_local::<T>()` reads Col 0 (Cap'n Proto) first. Snapshot bytes pass through `schema_version::unpack_versioned(...)`, which requires an exact schema-version match; older and newer versions return `SchemaVersionError::Older` and `SchemaVersionError::Newer`, respectively. If Col 0 is missing, it returns `Default`.

**Write path**: `save_local::<T>()` writes Cap'n Proto snapshot to Col 0. Col 1 (automerge doc) is reconciled on-demand before merge/sync operations.

**Merge path**: `pull_and_merge::<T>()` loads the remote automerge doc from DHT, merges with the local automerge doc via `doc.merge()` (true CRDT semantics — no manual `Mergeable` trait), then saves both columns if the merged state differs from local.

**DHT path**: the subkey payload is those same doc bytes — the DEFLATE form — so the wire copy shrinks with the row (2.2×–3.5× smaller than the raw form), which matters where it sits against Veilid's 32 768-byte subkey ceiling. Nothing on the write path needs to know: the doc is written as returned by `save()`, and every reader goes through `AutoCommit::load`, which accepts either form.

**Publish fold-back**: a publish writes the model to the DHT and folds the written doc back into the local row. When the model has **no local row yet**, the fold is `seed_local_doc()`: it stores the network's own doc as the local doc. That is not bookkeeping — without it the local row's doc would be built from the snapshot alone, on an **independent automerge root**. Automerge replaces a whole map object when two independent roots are merged, so the next merge against the network's doc would keep one side's map whole and drop the other's keys: a contact written locally vanished on the next publish. Seeding the row from the network's doc makes the local doc descend from the network's history, so the merge is a fast-forward and both sides' keys survive.

### Auto-Compaction

Automerge documents grow with every edit (they retain full change history). To prevent unbounded growth, `reconcile_row_doc::<T>()` gates a compaction attempt on **two** conditions, both measured on the freshly reconciled doc's **raw** (uncompressed, `save_nocompress()`) serialization length:

> `raw_len > DEFAULT_COMPACTION_THRESHOLD` **and** `raw_len > watermark + DEFAULT_COMPACTION_THRESHOLD`

`watermark` is the length recorded at the row's last successful compaction (`ReservedMeta::CompactionWatermark`, `0` when never compacted). The unit is deliberate: a dual-format row *stores* the doc in automerge's DEFLATE form (`save()`), 2.2×–3.5× smaller than the raw form (see [compression.md](compression.md)), and both `DEFAULT_COMPACTION_THRESHOLD` (16 384 B) and the watermark are in **raw doc bytes** — the unit they have always had. Keeping the trigger on the raw length is what stops the stored form from silently redefining the threshold as "16 KB of DEFLATE" and dropping the watermark one compression ratio below every later measurement, where it would gate nothing.

Measuring the raw length also means a completed compaction does not lower the measured length, which is the whole point of the watermark: a plain "over threshold" check would stay true forever and re-compact the doc on every mutation→publish cycle. The reconcile reads that length from `reconcile_to_automerge_measured` — the doc is in memory there anyway, and the extra `save_nocompress()` is an order of magnitude cheaper than the `save()` beside it.

The attempt is skipped when the doc is at or below the threshold, or has grown less than a threshold past the watermark — i.e. at most one compaction per `DEFAULT_COMPACTION_THRESHOLD` raw bytes of doc growth (~165 added messages at ~99 B of doc per message), instead of one per publish. A skipped attempt records no snapshot entry and rewrites nothing; the state it would have recorded is still restorable from the newest retained entry, and the retainable set is unchanged — only a *completed* compaction appends (and thus can prune).

When it does run, `snapshots::maybe_compact::<T>()` takes the row's store lock and re-reads the doc and currentness marker, proceeding only if they still hold exactly the bytes and marker the trigger described — the same optimistic check the merge paths use, so a row replaced mid-flight is never overwritten with the older doc:

- **Records the current Cap'n Proto snapshot** (timestamp, packed size, and the payload itself) in the `po_snapshots` history index — unless the newest retained entry already holds **byte-identical** payload bytes, in which case the append is skipped entirely (a duplicate would add no restorable state and, under `MAX_SNAPSHOTS`, could evict a distinct entry to make room for it; retention is deferred to the next distinct append or `apply_retention`, never changed).
- **Writes the doc back in the form it compared it against** — the stored DEFLATE form — in the same `apply_batch` as the watermark. There is no second form to convert to: re-serializing a doc already in the stored form is a byte fixed point (`load(x).save() == x`, verified for both forms), so compaction no longer rewrites the doc to a different encoding. Nothing is discarded: history and object identities are untouched, and it never replaces the doc with a fresh current-state-only document.
- **Advances the watermark** (in raw doc bytes) in that same batch, so a failed write cannot leave a watermark behind a doc it never described.
- **Prunes** expired history entries per the user's retention policy and the configured maximum, deleting their payloads.

Compaction preserves the Automerge causal history/object identities, so ordinary cross-device CRDT merges continue to use the existing history rather than a fresh-root document.

### Reserved metadata & row wipes

A dual-format row is not just `(col 0 snapshot, col 1 doc)`: `COL_AUTOMERGE` also carries reserved metadata entries — the currentness marker and the compaction watermark — enumerated once in `store_trait::ReservedMeta`. Every wipe derives from that enumeration:

- `store_trait::delete_row(table, key)` deletes the snapshot, the doc, and **all** reserved entries for a row in a single `apply_batch` (callers: account cleanup, conversation delete).
- Account-wide wipes (`chat/storage/crud.rs::collect_account_deletes`) filter `COL_AUTOMERGE` by the reserved-key prefixes under the account's row-key prefix, so orphaned sidecars cannot survive a delete or a backup replace.

Never hand-roll a delete of these keys: a partially wiped row (snapshot gone, doc left) is reconciled straight back and re-marked current against a re-created snapshot.

### Snapshots

`po_snapshots` is a two-column TableDB table of per-model/account history. Column 0 holds the snapshot index (initial binary format: `INDEX_FORMAT_VERSION = 1` version byte, count, `[timestamp, size]` pairs); column 1 holds each entry's restorable Cap'n Proto payload, keyed `model_id\0account_key\0timestamp_be` and zstd (`ZSTD_LEVEL` = 12) compressed through the `table_store::Table::store` API like every other TableDB row. Key features:

- **Time travel**: `list_snapshots::<T>(account_key)` lists the retained entries from the index — no payload is inflated just to be listed. `restore_to_snapshot::<T>(account_key, timestamp)` restores a retained payload. Each snapshot's payload row is written **before** its index entry, so a failed payload write drops the entry instead of listing one that cannot be restored; a retained payload whose Cap'n Proto schema version differs from the running one fails with a downcastable `SchemaVersionError` (`Older`/`Newer`).
- **Restore semantics**: restore decodes the payload via `decode_snapshot`, captures the pre-restore state as a new snapshot entry (so moving forward again is just restoring to a later entry in the list), then swaps state atomically via `replace_local`, which reconciles the restored value **into the retained automerge doc** and commits snapshot + doc + currentness marker in one transaction. Entries absent from the restored state become real CRDT delete operations, causally after the inserts the doc already merged, so the next publish deletes them on every device instead of resurrecting them — reconciling a fresh-root doc instead would race the DHT copy on `(lamport, actor)` and silently keep one root wholesale. Entries created concurrently on another device that this doc has never merged still survive (normal CRDT semantics). The backup-import Replace path (`import_all_raw`) applies the same reconcile-into-doc rule to the `ConversationIndex` row, in the same transaction as its message rows. A restore that matches the current state is a no-op.
- **DHT convergence**: restoring does not enqueue sync itself; the caller re-enqueues the model's `OP_TYPE`, so the restored state re-publishes to the DHT and other devices converge to it.
- **Manual entry**: `create_manual_snapshot::<T>()` records the entry and retains its payload.
- **Retention policy** (`RetentionPolicy`): `Days30` (default), `Days90`, or `Forever`. Applied during compaction and on demand by `apply_retention_all(account_key)`, which calls `apply_retention::<T>` once per surface in a fixed order (`TotpTokenList`, `ContactList`, `ConversationIndex`, `AppState`) — the settings UI's single call site. Both paths prune expired entries **and** delete their payloads (no orphaned payload rows); each surface commits separately, so a mid-way failure leaves the earlier surfaces correctly pruned.
- **Duplicate suppression**: an append whose payload bytes match the newest retained entry is dropped before anything is written (see Auto-Compaction above); it can never evict a distinct entry.
- **Hard cap**: Maximum 100 entries per model per user, enforced regardless of retention policy.

## Important Invariants

- **All writes go through `with_record`**: DHT record open/close lifecycle is managed centrally with a 30s timeout.
- **`is_current()` check**: Guards pre-dispatch, success, and failure paths with current `id` + `sequence_number` ownership so replaced ops neither reach handlers nor mutate newer queue state.
- **`ProcessingGuard`**: RAII guard ensures `IS_PROCESSING` resets even on panic.
- **`scheduled_at` field**: Enables future-scheduled retries without busy-waiting. The drain loop sleeps until the next scheduled op.
- **Network ownership**: The app queue always dispatches ready work. Veilid buffers offline writes, and failures without usable connectivity do not consume retry budget.
- **Dual-format consistency**: `save_local` writes only the Col 0 snapshot. Automerge reconciliation and `pull_and_merge` update Col 1 (and both columns when merged remote state differs).

## How to Add a New Sync Operation Type

### For child-record models (preferred — use `CrdtModel`):

1. **Implement `CrdtModel`** for your type (see `storage/store_trait/` for the trait definition: `SyncSurface::ID`, `DISPLAY_NAME`, `TABLE_NAME`, `OP_TYPE`, `pack()`, and `unpack()`).
2. **Add an `OpType` variant** for `YourType::OP_TYPE` in `pending_operation.rs`.
3. **Register the operation** in `user/sync_ops.rs::publish_registered_op`, mapping it to `id.sync_child_crdt::<YourType>()`. This static surface map is not populated by the macro.
4. **Use `mutate::<YourType, _>(&ctx, ...)`** from `sync/mutate.rs` for mutations. It handles lock → load → closure → save → enqueue.
5. **Update docs**: Add your type to the dispatch table above and the child record table in `dht-architecture.md`.

### For non-model ops (identity subkeys, conversation ops, etc.):

1. **Add the `OpType` variant** in `models/pending_operation.rs`.
2. **Implement and dispatch the operation** — add an explicit handler in `sync/handler.rs` for composite/non-surface flows; surface-backed ops use `publish_registered_op()` instead.
3. **Assign queue policy** in `sync/queue_policy.rs` (lane family and retry schedule).
4. **Squashing**: Every operation deduplicates by `(account_id, OpType, target)`. For `ConvFeedSync`, squashing unions the covered message-ID sets; for all other ops, the newest pending row replaces the prior owner while keeping its own fresh queue identity. An op addressed by two keys (peer + conversation) uses `ctx.enqueue_for_scoped_target(...)` and a `DedupTarget::Invite` — keep the two keys in their own fields, never joined into one string: a Veilid record key contains colons of its own.
5. **Enqueue from your mutation site**: Use `ctx.enqueue(YourOpType, payload)` (identity-gated via `OpType::requires_identity()`) or `ctx.enqueue_for_target(YourOpType, payload, target_key)` from `UserCtx`, or the `mutate::<T>()` API which handles enqueue automatically.
6. **Update docs**: Add to the dispatch table above and `README.md` if it uses a new subkey.
