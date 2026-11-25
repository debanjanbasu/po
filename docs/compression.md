# Compression System Architecture

**Owns:** `src/compression/`, `src/compression/training_queue/`

The compression system provides versioned zstd compression with trained dictionaries, optimized for structured records (profiles, settings, config, chat data).

## Key Files

| File | Purpose |
|------|---------|
| `build.rs` | Build-time schema compilation, about metadata generation, and embedded structured dictionary bootstrap with cache/empty-corpus skip warnings |
| `src/compression/codec/mod.rs` | Pooled zstd compressor/decompressor contexts, blocking kernels, and async entry points |
| `src/compression/codec/dictionary_id.rs` | Plain/content `DictionaryId` identities, structured-frame headers, and `b3:<digest>` display/parsing |
| `src/compression/codec/registry.rs` | In-memory `DictionaryRegistry`, `TableDB` dictionary persistence, and activation |
| `src/compression/trainer.rs` | Background dictionary training (1hr min interval) |
| `src/compression/training_queue/mod.rs` | Training queue facade and `TableDB` read/write + stale-rewrite handling |
| `src/compression/training_queue/patterns.rs` | Aggregated pattern model + training sample conversion |
| `src/compression/training_queue/serialization.rs` | Cap'n Proto pack/unpack helpers for queue payloads |
| `src/compression/samples.rs` | LRU sample collection with auto-flush |
| `src/compression/recompression.rs` | Migrates old data to current dictionary version |
| `src/veilid/dict_sync.rs` | Cross-device dictionary sync via DHT subkeys 5–6 |
| `src/storage/mod.rs` (`training_status`, `upgrade_status`) | `TRAINING_STATUS` and `UPGRADE_STATUS` global signals |
| `src/storage/automerge.rs` | Automerge doc producers — the single stored form (automerge DEFLATE) this layer then compresses |
| `src/components/overlays/activity_bar.rs` | Compact status surface for training/recompression indicators |
| `src/components/overlays/activity_modal.rs` | Detailed background-task status (training + migration progress) |

## Compression Modes

Data is compressed in one of two modes:

| Mode | Dictionary | Typical Data |
|------|------------|-------------|
| Structured (`compress_structured`) | Single trained dictionary (from both structured + chat samples) | Profiles, security settings, app state, CRDT metadata, chat messages |
| Plain (`compress_plain`) | None — raw zstd | Keypair blobs, dictionary blobs, pairing beacons, local dictionaries |

There is **one unified dictionary** (`assets/dict/structured.zdict`) trained from both structured field patterns and chat message patterns. Plain compression uses raw zstd with no dictionary. All compression across the app (local `TableDB`, DHT transport, dictionary blobs, recompression, and backup exports) uses `ZSTD_LEVEL = 12`. Structured compression uses the trained dictionary selected from the `DictionaryRegistry`.

**Level 22 → 12 (2026-11-12).** Every measured table below was taken at level 22 on native, before that change; the sizes are ~1-3% smaller than the shipped build produces and the CPU figures are ceilings roughly 9× above it. The change was made because web runs the codec inline on the UI thread — see `config::compression::ZSTD_LEVEL` and commit `0187f38e`, which measured level 12 as byte-identical to 19 on all seven reference payloads at 11–18% of the CPU.

At build time, `build.rs` filters blank/whitespace-only/comment-only text lines and zero-length binary entries before retraining the embedded dictionary. An empty filtered corpus warns and preserves the current dictionary; a matching sample hash silently reuses the cached dictionary.

Cargo disables zstd's legacy-frame, fixed-array, and build-time-bindgen features.
All targets retain `zdict_builder`; native targets additionally enable `zstdmt`.
`zstd-sys` remains transitive and disables assembly automatically on WASM.

## Automerge documents: one stored form

Automerge CRDT documents (see [sync-pipeline.md](sync-pipeline.md)) are **not** stored as raw document bytes. Every producer in `src/storage/automerge.rs` calls `AutoCommit::save()` — automerge's own DEFLATE output — and the table layer then compresses that with structured zstd at `ZSTD_LEVEL` exactly like any other row. That is the single stored form, local and on the DHT.

Skipping automerge's DEFLATE (the pre-change `save_nocompress()` form) only moves the work downstream: zstd then runs over a 2.2×–3.5× larger input on every write and stores a *larger* row — and a chat conversation doc is rewritten on every message send. Measured on conversation documents (automerge 0.12 + autosurgeon 0.14, accumulated CRDT history, the trained `assets/dict/structured.zdict` — **native, level 22**; see the level-change note above):

| doc | `save_nocompress()` | `save()` (DEFLATE) | zstd L22(dict, `save_nocompress()`) | zstd L22(dict, `save()`) |
|-----|--------------------|--------------------|-----------------------------------|------------------------|
| 20 messages | 3 733 B | 1 727 B | 1 639 B | **1 587 B** |
| 200 messages | 34 656 B | 10 280 B | 11 071 B | **10 294 B** |
| 1 200 messages | 203 109 B | 57 374 B | 60 306 B | **57 388 B** |

Full write-path totals (doc serialization + the table layer's zstd, native level 22):

| doc | raw + zstd L22 | DEFLATE + zstd L22 |
|-----|----------------|--------------------|
| 200 messages | 3 463 µs / 11 071 B | **614 µs / 10 294 B** |
| 1 200 messages | 18 758 µs / 60 306 B | **3 233 µs / 57 388 B** |

~5.7× cheaper and 5–7% smaller. **The trained dictionary does not earn its keep on these rows** — with and without it the sizes land within 5% (0.95–1.00×), because automerge already removed the redundancy zstd's dictionary would model. It earns its keep on the other structured rows listed above; do not expect the same ratio here.

`AutoCommit::load` reads either form (automerge's storage format is self-describing: the deflate bit lives in the column specification), so rows written before this rule keep loading — a row still holding the old form is rewritten in the new one the next time it is reconciled. The one place the raw length is still needed is the compaction trigger, whose threshold and watermark are defined in *raw* doc bytes: `reconcile_to_automerge_measured()` returns the stored bytes plus that raw length, and `snapshots::maybe_compact` keeps the two units apart. Do not "fix" the compaction trigger to measure the stored bytes; see [sync-pipeline.md](sync-pipeline.md#auto-compaction).

## Blob Framing

Structured frames carry a `DictionaryId` header naming the dictionary that compressed them. The supported structured headers are version-zero transport frames and content-identified local frames:

### Version-zero transport frames (all DHT writes)

Every DHT write — identity, child, and shared conversation values — uses the stable transport framing: version zero followed by a **dictionary-free** zstd frame:

```
┌────────────────────┬──────────────────────────────┐
│ version: u64 (LE)  │ dictionary-free zstd frame   │
│ 0 (8 bytes)        │ variable length              │
└────────────────────┴──────────────────────────────┘
```

Retained network history never depends on a device's learned dictionary window: decoding a transport frame never touches the private dictionary store. After the header, bytes are AEAD-encrypted as usual (`StructuredEncrypted` store mode).

### Content-identified local frames (learned content)

Locally compressed structured content names its dictionary by complete VLD0/BLAKE3 digest:

```
┌────────────────────┬────────────────────┬──────────────────────┐
│ marker: u64 (LE)   │ digest: [u8; 32]   │ compressed_data: [u8]│
│ u64::MAX (8 bytes) │ 32 bytes           │ variable length      │
└────────────────────┴────────────────────┴──────────────────────┘
```

The digest is the dictionary's content identity (`b3:<64 lowercase hex>` via `DictionaryId`'s `Display`/`FromStr`). The same identity always refers to the same bytes — a collision is refused, never silently replaced. Each structured write takes one atomic `(DictionaryId, Option<Arc<PreparedDict>>)` snapshot before compression so the header and compressor dictionary cannot disagree during a concurrent dictionary swap.


### Plain blobs

`Plain` blobs are raw zstd frames with **no** header and **no** dictionary lookup on either the write or read path.

**Constants:** `VERSION_HEADER_BYTES = size_of::<u64>()` (8 bytes; content frames add `HASH_BYTES` = 32 for a 40-byte header).

## Dictionary Registry

The `DictionaryRegistry` is a global singleton behind standard-library read/write locks, initialized once at startup and reloaded from TableDB after Veilid is available.

**Key behaviors:**

- Trained dictionaries are keyed by content identity. Persisted rows in `po_dicts` are immutable content-addressed blobs (`structured.vb3:<digest>`); numeric dictionary identities and the old `structured.v1` format are not read or migrated.
- Holds up to **8 hot dictionary versions** in memory (`MAX_DICT_VERSIONS`) as `PreparedDict` entries (`Arc<[u8]>` bytes + pre-parsed `dict_id`). When exceeded, the oldest in-memory version is evicted in insertion order; historical content-addressed blobs remain in persistent `po_dicts` storage.
- Startup hashes the embedded `STRUCTURED_ZDICT`, stores its `PreparedDict` under its content identity, and loads the active content-only pointer from `active_dictionary`. After Veilid is available, `reload_from_tablestore()`—guarded by `DICTIONARIES_READY`—completes before dependent local I/O; older persisted content versions are lazy-loaded on demand.
- Thread-safe reads/writes use the registry helpers. Compressor/decompressor contexts are pooled by `DictionaryId` (`CODEC_POOLS`, `MAX_IDLE_CONTEXTS_PER_POOL = 4` per `DictionaryId`), with all compressors operating at `ZSTD_LEVEL = 12`.

## Compression Flow

```
compress(data, domain)
        │
        ├── domain = Structured
        │       │
        │       ├── one-time TableDB reload (DICTIONARIES_READY bootstrap)
        │       ├── read_registry() once → atomic (DictionaryId, Arc<PreparedDict>) snapshot
        │       ├── acquire pooled Compressor for DictionaryId
        │       ├── compress with snapshot dictionary at `ZSTD_LEVEL` (12)
        │       └── prepend DictionaryId header → Bytes
        │
        └── domain = Plain
                │
                ├── acquire pooled plain Compressor (DictionaryId::Plain)
                └── raw zstd frame → Bytes
```

## Decompression Flow

```
decompress(data, domain)
        │
        ├── empty data → return empty Bytes
        │
        ├── domain = Plain
        │       │
        │       ├── use raw payload as-is
        │       └── acquire pooled Decompressor (dictionary cleared)
        │
        └── domain = Structured
                │
                │   (one-time TableDB reload via DICTIONARIES_READY)
                ├── read the `DictionaryId` header (`load_frame_dictionary` resolves
                │   version-zero transport or content identity; numeric identities
                │   are rejected)
                ├── ensure the named `PreparedDict` is loaded in-memory
                │   (lazy-load from `po_dicts` if needed; content frames must
                │   match the frame's digest — collisions are refused)
                └── acquire pooled Decompressor, bind DecoderDictionary (O(1)),
                     and decode with the exact-version dictionary
```

**Constant:** `MAX_DECOMPRESSED_BYTES = 10 MB` — safety limit for decompression output.

## Dictionary Training Pipeline

Training is a multi-stage pipeline that runs entirely in the background without blocking the UI.

```
 User data (messages, profile saves)
        │
        ▼
 ┌─────────────────┐
 │ Pattern          │  patterns.rs
 │ Extraction       │  Extract n-grams, emojis, templates.
 │                  │  Content-bearing patterns; account-scoped persistence.
 └────────┬────────┘
          ▼
 ┌─────────────────┐
 │ Sample           │  samples.rs
 │ Collector        │  LRU cache (10K entries), dedup via hash.
 │                  │  Auto-flush at 100 pending samples.
 └────────┬────────┘
          │ flush()
          ▼
 ┌─────────────────┐
  │ Training         │  training_queue/mod.rs (+ split modules)
 │ Queue            │  Persists AggregatedPatterns to TableDB.
 │                  │  Local aggregate merge; packed Cap'n Proto snapshots.
 └────────┬────────┘
          │ TRAIN_NOTIFY
          ▼
 ┌─────────────────┐
 │ Trainer          │  trainer.rs
 │                  │  Wakes on TRAIN_NOTIFY; a too-soon wake re-arms
 │                  │  for the remaining interval, else no timer at all.
 │                  │  Guards: ≥1000 samples, ≥1hr since last training.
 │                  │  CPU work in spawn_blocking (local, no network gate).
 └────────┬────────┘
          │ swap dict
          ▼
  ┌─────────────────┐
  │ Registry        │  codec/registry.rs
  │ Activation      │  Persists the content-addressed dictionary to
  │                 │  TableDB, activates it durable-first in
  │                 │  DictionaryRegistry (DICTIONARY_UPDATE_LOCK).
  └────────┬────────┘
           │ RECOMPRESS_NOTIFY
           ▼
  ┌─────────────────┐
  │ Recompressor     │  recompression.rs
  │                  │  Migrates old-version data in TableDB.
  └────────┬────────┘
           │ enqueue DictPublish
           ▼
  ┌─────────────────┐
  │ DHT Publish      │  dict_sync.rs
  │                  │  One DHT transaction writes subkey 5 metadata
  │                  │  (active `dict_id`) and subkey 6 (current blob).
  └──────────────────┘
```

### Stage 1: Pattern Extraction (`training_queue/patterns.rs`)

Extracts content-bearing patterns from raw data:

- **Chat patterns:** Literal word/n-gram frequencies, emoji frequencies, and structural templates. Eligible short messages can remain recognizable in these samples.
- **Structured patterns:** Field key templates (values replaced with wildcards, e.g., `username\0*\0dob\0*`), key frequency maps.

These summaries are not anonymized or content-free. Treat persisted aggregates
and learned dictionaries as sensitive account data. Persisted aggregates are
capped to the top entries used by training (`word_freq` ≤ 5,000, `emoji_freq`
≤ 1,000, `templates` ≤ 500) on each flush and cleared on successful training
reset or explicit account cleanup, with no per-entry TTL.

### Stage 2: Sample Collection (`samples.rs`)

The `SampleCollector` is a process-global, single-account LRU behind a `Mutex`. The first sample for a different account clears the previous cache and advances `COLLECTOR_GENERATION`, so in-flight flush failures cannot restore another account's samples.

- **LRU cache** with 10,000 entries per domain (`LRU_CAPACITY`), deduplicating complete patterns by hash (`DefaultHasher`).
- **Auto-flush** at 100 pending samples (`SAMPLE_FLUSH_THRESHOLD`): spawns an async task that calls `flush()`.
- **`flush()`** captures the current `UserCtx` and `SessionGuard`, drains only that account's cache, and persists under the username row lock. A failed flush restores its drained batch only if the collector still belongs to the same account (`COLLECTOR_GENERATION` unchanged and account ID matches). If the account switched or the collector was reset while the flush was failing, the drained batch is not restored; the drop is reported via `report_nonfatal` so it appears in Activity instead of being silently lost.

Public API: `collect_chat(account_id, pattern)`, `collect_fields(account_id, keys)`.

### Stage 3: Training Queue (`training_queue/mod.rs`)

Persists `AggregatedPatterns` to a TableDB table (`po_learned_samples`, 3 columns):

| Column | Key | Content |
|--------|-----|---------|
| 0 | `account_id\0chat` | Chat `AggregatedPatterns` (packed Cap'n Proto) |
| 1 | `account_id\0structured` | Structured `AggregatedPatterns` (packed Cap'n Proto) |
| 2 | `account_id\0metadata` | Packed training metadata (last training timestamp) |

`AggregatedPatterns` contains `word_freq`, `emoji_freq`, `templates`, and
`last_updated`. These rows are not Automerge documents and are not cross-device
CRDT replicas. The schema's `type_freq` field is reserved and written empty.

All read/merge/write training mutations use an explicit captured account ID. `persist_samples`, `touch_last_training`, and `reset_after_training` acquire `UserCtx::lock_for_account_commit()` and hold the per-account username lock through the TableDB commit so concurrent flushes and account cleanup cannot race.
`patterns_to_training_samples()` converts aggregated patterns back into synthetic training samples for zstd's dictionary builder.

### Stage 4: Trainer (`trainer.rs`)

A background task spawned via `spawn_trainer()` that wakes on `TRAIN_NOTIFY` (set by the sample collector on flush — the only wake source; the fixed hourly fallback wake and its `TRAINING_INTERVAL_MS` constant are gone). Each `maybe_train` run returns the delay the loop arms next:

- **Too soon since `last_training`** → sleep exactly until `last_training + MIN_TRAINING_INTERVAL_SECS`, floored at `TRAIN_RETRY_FLOOR_MS` (60 s) so the epoch-second comparison cannot spin.
- **Nothing pending** (no active account, below `MIN_TRAINING_SAMPLES`, or a run just finished) → block on `TRAIN_NOTIFY` with no timer.
- **Failed attempt** (including a failed `last_training` read, which is what keeps failed runs debounced) → retry after that same 60 s floor rather than parking forever.
- **Web, page visible** (checked only once the threshold is met, so it costs nothing while nothing is trainable) → retry after `DEFERRED_RESUME_MS` (60 s). Training is zstd dictionary training over the whole aggregated corpus plus two comparison compressions, inline on the single thread web has; a visible page is a page that paints, and delaying the run costs nothing but a slightly older dictionary. The deferral is deliberately **not** stamped through `touch_last_training`: a deferral is not an attempt, so the interval is re-tested on the next pass instead of being debounced an hour for having looked.

Every wake re-enters `maybe_train`, which re-captures the `UserCtx`/`SessionGuard` and re-checks the epoch after each await; the task is `spawn_session_scoped`, so a logout cancels the pending wait instead of leaking it into the next session.

**Guards before training (`maybe_train` runs locally without a network attachment gate):**
- At least 1,000 total samples (`MIN_TRAINING_SAMPLES`).
- At least 1 hour since last training (`MIN_TRAINING_INTERVAL_SECS`).

**Training (`train_and_swap`):**
1. Load base samples from embedded text file (`assets/dict/samples/structured.txt`) containing the unified structured + chat corpus, ignoring blank/whitespace-only/comment-only lines.
2. Convert learned `AggregatedPatterns` into training samples.
3. Merge base + learned samples. If the merged corpus is empty after filtering, skip training.
4. `spawn_blocking`: call `zstd::dict::from_samples()` with `DICT_SIZE = 16,384` bytes for the unified Structured dictionary.
5. Validate improvement against current dictionaries using test data built from the top learned chat patterns (count desc, key asc, top 10; fixed chat lines if none were learned) plus fixed structured probes. Must exceed **1.0%** (`MIN_IMPROVEMENT_PERCENT`).
6. If improved: swap the dictionary for the new version.
7. Enqueue `OpType::DictPublish` for DHT sync.
8. Update `TRAINING_STATUS` (`InProgress` while training, `Idle { last_trained_epoch_ms }` on completion).

`maybe_train` captures one `UserCtx` and `SessionGuard`, uses that account ID for all queue reads, and holds the username lock through every metadata/pattern write. A stale run cannot reset another account's samples.

### Stage 5: Dictionary Activation (`codec/registry.rs`)

Activates the trained dictionary under its content identity:

1. Derives the `DictionaryId` from the trained bytes (`DictionaryId::for_dictionary`) — callers never allocate a numeric version.
2. Reloads from TableDB, takes `DICTIONARY_UPDATE_LOCK`, and adds the content-addressed dictionary to `po_dicts` (`structured.vb3:<digest>`), refusing identity collisions.
3. Stores the active content identity in `active_dictionary` and activates it in `DictionaryRegistry` (durable-first activation: the persisted row exists before the in-memory swap).
4. Fires `RECOMPRESS_NOTIFY` to wake the recompressor.

All subsequent `compress()` calls immediately use the new dictionary, whether it was trained locally or activated from a synced remote version through the same path.

## Recompression (`recompression.rs`)

When dictionaries are swapped, data compressed with old versions should be migrated.

**Counting:** Each cycle scans the structured local tables and counts well-framed rows whose dictionary identity differs from the active one; it does not keep a separate accumulated old-row count.

**Scan cost:** one `get_keys` listing per column of the 14 structured tables (24 columns) plus one raw `TableDB` read per existing key, which copies that row's compressed blob only to read its 8-byte frame header. No row is decompressed, and the scan retains only the `(column, key)` of a stale row — not its bytes, which would hold the whole table's stale payload for the length of a migration that typically touches thousands of rows. `migrate_table` reloads the one row it works on (`compare_and_swap_raw` re-reads it under the row lock anyway), and its `Bytes::clone` for that CAS is a refcount bump, not a copy. The hourly fallback is the only wake that can pay the scan for nothing, which is what the marker below removes.

**Trigger policy (`spawn_recompressor`):** Wakes on `RECOMPRESS_NOTIFY` or the 1-hour fallback.

**Web: the expensive half waits for a hidden page.** Past the marker check, a cycle is a full scan plus inline decompress/recompress per stale row — on the single thread web has, and `spawn_blocking` is a label there, not an offload. So on web the cycle returns early (`platform::lifecycle::defer_heavy_work`) while the page is visible, having recorded nothing: the work stays pending, and the loop's delay becomes `DEFERRED_RESUME_MS` (1 minute) instead of the hour, so the pass lands on the first hidden moment — backgrounding, tab switch, or screen lock. A no-op wake stays on the hourly cadence, because it never reaches the gate. Rows also yield a macrotask each (`sleep_ms(0)`), so a cycle interrupted by the page becoming visible again does not hold the first paint off for the length of a burst.

- **Dictionary activation is the primary trigger and always forces a scan.** `notify_recompress()` bumps an in-memory generation, so a scan runs even when the active identity comes back to the one already scanned (a swap chain that returns to it) and when the caller is not a swap at all.
- **A fallback wake is a no-op** — no table listed, no row read — while *both* of these hold:

| Condition | Source |
|-----------|--------|
| No activation since the last completed scan | `RECOMPRESS_GENERATION` vs `SCANNED_GENERATION`, in-memory, baselined in `spawn_recompressor` (so the startup dictionary load and pre-login activations are not treated as swaps) |
| The active identity is the one the last completed scan ran under | `scanned_dictionary` row in `po_dicts`, same table and `b3:<digest>` text form as `active_dictionary` |

The marker is written only by a scan that left no stale row behind, so a row whose old dictionary is unrecoverable keeps the fallback retrying it. It is unreadable → treated as absent (scan runs; the failure is reported). It lives beside the dictionary blobs it names and outside the scanned tables, so account teardown neither orphans it nor strands a scan: deleting an account removes account rows from the scanned tables while the device's active dictionary identity is unchanged, and rows written afterwards are compressed with that active dictionary. Swaps can only happen in-process (activation persists `active_dictionary` first), so nothing is missed while the app is closed.

Each cycle that does run:

1. Scan structured local tables and count old-version rows; return immediately if none are stale — recording the marker first, since the dictionary is fully covered.
2. Set the upgrade-status UI indicator (`UPGRADE_STATUS`) with the count.
3. Iterate the structured local tables (`po_learned_samples`, `po_identity_docs`, user/session/device storage, CRDT/local-conversation tables, pending-op/invite trackers, snapshots, failed-hydration tracker, etc.) — the scan and the per-row recompress both fan out unbounded via `helpers::concurrency::map_concurrent` (see [docs/concurrency.md](concurrency.md); each row serialises on its own store lock, so the fan-out adds no shared contention).
4. Recompress rows outside their locks, then compare-and-swap against the exact bytes read before compression. Changed rows are left untouched; malformed or unrecoverable rows are reported and skipped.
5. Future cycles recount from storage. Historical dictionary blobs remain in local `po_dicts` for lazy loading; cleanup is deferred.

`UPGRADE_STATUS` is set/advanced/cleared during migration so UI surfaces can show live progress (`total`, `completed`).

**Why the marker is sound:** every row of every scanned table is written through `Table::store` / `apply_batch` / `store_if_absent` (`compare_and_swap_raw` writes bytes the recompressor just produced), and all of those frame with the active dictionary at write time. Remote data arrives as decoded Automerge/Cap'n Proto and is re-framed locally; backup and snapshot restores read through the decompressing accessors (`export_all_raw` → `load_opt`) and write back through `apply_batch`. So a row's frame identity equals the dictionary active when it was written — and while the active identity is still the one a completed scan ran under, no row written since that scan can be stale. Rows written *before* it were checked by it.

**Not visible to the marker:** a swap chain that returns the active identity to the last completed scan's identity while the scan of the intermediate identity was itself incomplete (a row left behind), separated by a process restart — the in-memory generation that would catch it in-process is gone, and the identities match. Recovery is then deferred to the next activation. Reaching it needs a row that failed recompression (a locally written row always names a dictionary that was persisted to `po_dicts` before activation, so this means a row with a valid header and a corrupt body) plus two activations and a restart. Accounts and snapshots are unaffected: neither teardown nor restore re-frames rows under another dictionary.

## Cross-Device Dictionary Sync (`dict_sync.rs`)

Dictionaries sync across devices via two DHT identity-record subkeys:

| Subkey | Content | Encryption |
|--------|---------|------------|
| 5 | Dictionary metadata (Automerge): active `dict_id` content identity (`b3:<digest>`) | AEAD + structured-transport zstd (dictionary-free) |
| 6 | Current trained dictionary blob — standalone, validated by its content digest before install | AEAD + plain zstd (no dict) |

Dictionary blobs use `tx.set_encrypted_no_dict` / `store::load_opt(..., StoreMode::PlainEncrypted)` so loading a dictionary never depends on that dictionary.

### Publishing (`publish_to_dht`)

Triggered by `OpType::DictPublish` after training. One `with_record` transaction:

1. Reload the active dictionary and require a content identity.
2. Read existing subkey 5 metadata, set its `dict_id` to the active digest, and encode the updated Automerge document.
3. Atomically write subkey 6 (current blob, plain zstd + AEAD) and subkey 5 (metadata).

A superseded or oversized publication leaves the prior announcement and blob intact.

### Initial announcement (`write_initial_dictionary`)

The **identity record itself** carries the dictionary from the moment it is
created: `create_identity_record` writes subkey 5 and subkey 6 alongside the
profile, security, app-state and pointers subkeys.

Without this, a new account's record had no dictionary announcement until its
first training run — up to an hour later — and any device that paired during
that window found an empty subkey 5, had nothing to converge on, and stayed on
its own embedded dictionary.

These are first-write-only subkeys, so they use plain writes rather than a
transaction: a later publication goes through `publish_to_dht`, which
reconciles the existing metadata.

### Syncing (`check_and_sync`)

Called by the identity-record watcher, by the startup resync, and by the
post-pairing hydration. Within one `with_record` scope:

1. Force-read structured metadata from subkey 5 and parse its content identity.
2. If that dictionary is already local, use it; otherwise force-read the standalone subkey 6 blob, validate/install it under the announced digest, and activate it.
3. If the remote bytes differ from the active local dictionary, activate the remote content identity through `activate_dict_version()` so recompression is notified.
4. Report convergence (`report_dict_converged`) — this device now runs the dictionary the network announces.

## How to Add a Compression Domain

> In most cases, do **not** add a domain. Reuse `Structured` for structured/text data and `Plain` for data that does not benefit from the unified dictionary.

Compression dispatch is handled in `src/compression/codec/mod.rs` for both structured and plain modes (`compress_structured` / `compress_plain` and `decompress_structured` / `decompress_plain`). There is no domain `prefix()` or domain-specific TableDB key scheme. `get_dict_arc()` looks up a dictionary by `DictionaryId`, not by compression domain.

The current trainer, registry, persistence, and DHT sync support one unified trained dictionary. A domain needing an independent trained dictionary therefore requires a coordinated redesign of dictionary selection, persistence, training, and sync; adding an enum variant alone does not provide that support.

## Multithreaded Compression (`zstdmt`)

The native (non-wasm) `zstd` dependency compiles with the `zstdmt` feature. Exactly **one call site activates it: backup export** (`src/storage/backup.rs::compress_payload`), and only for payloads ≥ 8 MiB:

```rust
use zstd::zstd_safe::CParameter;
let mut c = Compressor::new(config::compression::ZSTD_LEVEL)?;
c.set_parameter(CParameter::NbWorkers(workers))?; // requires the `zstdmt` cargo feature
```

**Why nowhere else:** zstd multithreading splits *one input* into ≥512 KiB jobs — inputs below one job size get zero parallelism and pay scheduling overhead. Everything else the app compresses is DHT-subkey-sized (Veilid caps subkey values at 32 KiB). Throughput for many-small-payloads already comes from `spawn_blocking` + pooled codec contexts + parallel sync lanes. zstd decompression is single-threaded regardless of this feature — do not turn it on for the pooled DHT codec paths.

**Scope:** native targets only. The wasm target has no threads, so its `zstd` dependency deliberately omits `zstdmt` — the backup exporter's `NbWorkers` call is cfg-gated to non-wasm and wasm exports compress single-threaded. `[build-dependencies]` compiles its own `zstd-sys` from a narrower feature set — `zdict_builder` alone, the dictionary-training API — because Cargo resolves a build script's features separately from the target's: the trainer sets no `NbWorkers`, so it wants no `zstdmt`, and `fat-lto` there emitted LLVM bitcode objects that CI's Web-lint job could not link (`Cargo.toml`'s comment on that line records the `libzstd_sys-*.rlib: file format not recognized` failure). The shipped native compression path keeps both.

## Encrypted Backup Format (`src/storage/backup.rs`)

Backups (Settings → Data & Backups) use plain zstd frames independently of the DHT codec — no dictionaries or custom zstd framing. Inside the AEAD ciphertext the section/entry layout is length-prefixed (u32 LE lengths, strings via `push_str`) and is written/read only through the shared `helpers::framing` module. The POBK header version selects the backup format and is authenticated for v2 and v3.

```
[4B magic "POBK"][4B version u32 LE][nonce-prefixed AEAD ciphertext]
```

| Version | Payload inside AEAD ciphertext | Header authenticated? |
|---------|-------------------------------------|----------------------|
| v1 | Historical raw section payload (uncompressed) — unsupported | No |
| v2 | zstd-compressed payload; account-agnostic DHT key — unsupported | Yes |
| v3 | zstd-compressed payload; account-bound DHT key — current | Yes |

Compress-then-encrypt: zstd runs before AEAD; encrypted bytes do not compress. For v3, the 8-byte plaintext header (`magic ‖ version`) is AEAD associated data, so changing the version fails authentication. Import bounds section/item counts before allocation and rejects duplicate sections, duplicate KV keys, and trailing bytes.

Import accepts **v3 only**. This release does not migrate v1 or v2 backups; use a compatible older build to read and re-export them rather than editing version bytes.

Constants live in `config::backup`:

| Constant | Value | Purpose |
|----------|-------|---------|
| `MT_THRESHOLD_BYTES` | 8 MiB | Payloads at/above this engage `zstdmt` workers |
| `MT_MAX_WORKERS` | 4 | Worker cap (also bounded by available parallelism) |
| `MAX_DECOMPRESSED_BYTES` | 256 MiB native / **32 MiB web** | Import bound, checked against the zstd frame's declared content size before allocating. On web the import path holds ≈`2×file + 2×plaintext` at once in a heap that never shrinks, so the cap is set by what a tab survives, not by what the format allows |
| `MAX_FILE_BYTES` | 272 MiB native / **34 MiB web** | Max backup file size — checked in the file picker before reading and again in `import_backup`. Web mirrors the native relationship to `MAX_DECOMPRESSED_BYTES` at its own scale |

| `MAX_SECTIONS` | 16 | Max sections per backup — enforced before allocation |
| `MAX_KV_ITEMS` | 100,000 | Max rows in the conversations-data section — enforced before allocation |

Both size caps are also enforced on **export** (`storage::backup::ExportError`): a file this build cannot open again is not a backup. On web a larger payload used to mean a multi-second freeze on the UI thread and a possible heap trap, and the file it produced could not be imported anyway. The refusal names the size, the limit and the way out — the measured reasoning sits with the constants in `src/config.rs`.

### Restore Semantics (`src/components/settings/snapshots/`)

Restore is **plan-then-apply**: every section is decoded and validated (including
each conversation row) before anything mutates, so a malformed or mislabeled
backup fails with local state untouched. Structural rules fail closed:
unknown/duplicate sections abort; conversation data requires the conversation
index; a backup carrying identity restores only into the exact account
(username + account ID + DHT key) it was exported from.

Apply order: identity (failure aborts the whole restore) → conversations →
TOTP → contacts. The `ConversationIndex` row and per-conversation rows share
one table (`po_conversations`: index key = bare account ID, rows =
`account_id\0manifest_key`), so a Replace restore of a full backup commits the
index and **all** message rows in a single `TableDB` transaction
(`import_all_raw`), holding the per-account store lock across scan → commit →
cache invalidation. Full exports always carry the conversations-data section
(even when empty), so its presence unambiguously means "wipe and replace local
rows"; index-only partial exports leave local message rows alone.

Both Replace-mode CRDT writes — `replace_local` for the standalone models and
`import_all_raw` for the `ConversationIndex` row in a full-backup restore —
reconcile the restored value **into** the model's retained automerge doc and
commit the snapshot, doc, and currentness marker in one transaction
(`import_all_raw` keeps the index and its message rows in the same batch).
Conversation message rows themselves have no CRDT publish surface, so only the
index row's doc is retained. Entries absent from the backup become real CRDT
deletes (causally after the inserts the doc already merged), so the next sync
deletes them on every device instead of reconciling a fresh-root doc against
the pre-restore DHT copy (which would resurrect deleted entries or let one root
win wholesale). Entries created concurrently on another device that the doc has
never merged still survive — normal CRDT semantics. Merge mode is additive:
CRDT merge for the index/models, add-only for message rows; a mid-merge I/O
failure can leave a merged index entry whose rows arrive later via DHT hydration
(recoverable by design).

## Constants Reference

All tunable constants are centralized in `src/config.rs` (`config::compression` module) unless noted otherwise.

| Constant | Value | Canonical Location | Purpose |
|----------|-------|-------------------|---------|
| `ZSTD_LEVEL` | 12 | `config.rs` | Unified zstd compression level across local storage, DHT transport, and backups (was 22; see the level-change note at the top) |
| `VERSION_HEADER_BYTES` | 8 | `codec/dictionary_id.rs` | Size of the structured-frame version prefix (content frames add a 32-byte digest) |
| `MAX_DICT_VERSIONS` | 8 | `config.rs` | Max hot dictionary versions kept in memory |
| `MAX_DECOMPRESSED_BYTES` | 10 MB | `config.rs` | Safety limit for decompression output |
| `SAMPLE_LRU_CAPACITY` | 10,000 | `config.rs` | Max entries per domain in sample collector |
| `SAMPLE_FLUSH_THRESHOLD` | 100 | `config.rs` | Pending samples before auto-flush |
| `MIN_TRAINING_SAMPLES` | 1,000 | `config.rs` | Minimum samples before training runs |
| `MIN_TRAINING_INTERVAL_SECS` | 3,600 | `config.rs` | Minimum seconds between training runs |
| `RECOMPRESSION_INTERVAL_MS` | 3,600,000 | `config.rs` | Idle fallback wake for the recompression cycle |
| `DEFERRED_RESUME_MS` | 60,000 | `config.rs` | How soon a heavy pass that deferred itself (visible web page) or stopped short asks to be resumed |
| `DICT_SIZE` | 16,384 | `config.rs` | Target dictionary size in bytes |
| `MIN_IMPROVEMENT_PERCENT` | 1.0% | `config.rs` | Minimum improvement to swap dictionaries |
| `SUBKEY_DICT_META` | 5 | `dict_sync.rs` | DHT subkey for dictionary content-identity metadata |
| `SUBKEY_DICT_BLOB` | 6 | `dict_sync.rs` | DHT subkey for the current dictionary blob |
