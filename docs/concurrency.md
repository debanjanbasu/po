# Concurrency Patterns & Safety

**Owns:** `src/session/`, `src/helpers/`

This document describes every concurrency primitive the application uses, why it exists, how to use it correctly, and what mistakes to avoid.

---

## Table of Contents

1. [Session Epoch Guard](#session-epoch-guard)
2. [Per-Key Store Lock](#per-key-store-lock)
3. [Spawn Patterns](#spawn-patterns)
4. [GlobalSignal Safety](#globalsignal-safety)
5. [Static Mutexes](#static-mutexes)
6. [Cross-Platform Time](#cross-platform-time)
7. [Anti-Patterns](#anti-patterns)
8. [Timer Discipline](#timer-discipline)

---

## Session Epoch Guard

**File:** `src/session/epoch.rs`

### Problem

After logout → login, async tasks from the *previous* session may still be in-flight (DHT pulls, feed merges, identity syncs). If they complete and write to `SESSION`, conversation metadata, or local storage, they corrupt the new session's state.

### Mechanism

A global `AtomicU64` counter using `SeqCst` ordering:

```
SESSION_EPOCH: AtomicU64 = 0
```

| Function | Behavior |
|----------|----------|
| `SessionGuard::capture()` | Snapshots the current epoch into a `Copy` guard |
| `guard.is_current()` | Returns `true` if the captured epoch still matches |
| `guard.commit(f)` | Runs `f` only if still current; returns whether it ran |
| `current_epoch()` / `advance_epoch()` | Raw counter access — reserved for value-level uses (e.g. `AtomicU64` swap dedup in the compression trainer); application admission and route gates carry `SessionGuard` |

`SessionGuard` is the only epoch-checking API. There is no free `is_current(u64)` function; carry the guard itself through async call chains instead of a raw `u64`.

### When the epoch advances

- **Login:** session admission advances the epoch before promoting `SESSION` to `Authenticated` (`session/phase.rs`)
- **Logout:** `log_out()` / session clear paths advance epoch before wiping authenticated state (`user/auth/`, `session/phase.rs`)

### Usage pattern

#### `SessionGuard`

`SessionGuard::capture()` snapshots the epoch at creation. `guard.commit(...)` executes only if the session is still current and returns `true` when it runs:

```rust
let guard = SessionGuard::capture();

// ... async work: DHT pulls, feed merges, etc. ...

if !guard.commit(|| {}) {
    return Ok(()); // Session changed — bail silently
}

// Safe to update authenticated session state, conversation metadata, local storage, etc.
session::phase::mutate_authenticated_user(|u| { /* ... */ });
```

For session-scoped tasks that should auto-cancel on logout, use `spawn_session_scoped`:

```rust
let guard = SessionGuard::capture();
spawn_session_scoped(async move {
    let data = pull_from_dht().await;
    guard.commit(|| {
        session::phase::mutate_authenticated_user(|u| { /* ... */ });
    });
});
```

When a task needs to hand the epoch to a helper, pass the `SessionGuard` itself (it is `Copy` and `PartialEq`) and call `guard.is_current()` at the write point — never pass a raw `u64`.

### Where it's used

- `user/watcher/` — guards identity record merges
- `chat/watcher/mod.rs` — guards feed/manifest change handlers
- `chat/hooks/mod.rs` — guards chat store updates and feed watches
- `chat/signal/dispatch.rs` — passes epoch into `PeerSignal` dispatch
- `app/startup.rs` — guards app-state hydration on startup

### ⚠ Rules

1. **Always capture before `.await`** — never call `current_epoch()` after the async work is done.
2. **Check before every write** — a single task may have multiple write points; check at each one.
3. **Return silently** — stale tasks are expected, not errors. Use `return Ok(())`, not `bail!`.

---

## Per-Key Store Lock

**File:** `src/helpers/store_lock.rs`

### Problem

Multiple async tasks can race on the same local record:

```
Task A: load(key) → modify → save(key)
Task B: load(key) → modify → save(key)
                                 ↑ overwrites A's changes
```

This is a classic read-modify-write race. It happens when two feed changes arrive for the same conversation, or two identity pulls overlap.

### Mechanism

A generic `KeyedMutex<K>` (`src/helpers/store_lock.rs`) — a `HashMap<K, Arc<tokio::sync::Mutex<()>>>` behind a synchronous `std::sync::Mutex` (with `recover_poison`). Each unique key gets its own async mutex. The outer registry lock is held only long enough to look up or insert the per-key `Arc` (`lock_borrowed` avoids allocating on hits); idle entries (no holder or waiter) are pruned periodically.

```rust
pub struct KeyedMutex<K> {
    inner: std::sync::Mutex<HashMap<K, Arc<tokio::sync::Mutex<()>>>>,
    accesses: AtomicUsize,
}
```

Two instances exist:

| Instance | Key type | Home | Serializes |
|----------|----------|------|-----------|
| `STORE_LOCKS` (via `lock_store(&[u8])`) | `Vec<u8>` | `helpers/store_lock.rs` | Local record load → modify → save |
| `RECORD_LOCKS` | `RecordKey` | `veilid/store/lifecycle.rs` | Whole Veilid open → use → close scopes per DHT record (opens/closes are global per record) |

### Usage pattern

#### Conversation state: `storage::mutate_for_account` (choke-point)

All conversation mutations go through `chat/storage/crud.rs::mutate_for_account`. It acquires the composite storage-key lock, loads from the in-memory cache or disk, applies your sync closure in place, and persists via `persist_locked`:

```rust
let changed = storage::mutate_for_account(&account_id, &manifest_key, |conv| {
    insert_sorted_message(Arc::make_mut(&mut conv.messages), msg);
    true // return false to skip the save
}).await?;
```

`save_for_account` (reserved for conversation creation/import within `crate::chat`) persists through `persist_merged`, which reloads the on-disk state under the lock and **merges instead of overwriting**: messages are unioned by dedup key, `my_feed_meta` resolves field-wise monotonically, and the roster resolves LWW by `last_updated` (`chat/storage/merge.rs`). Every writer must participate in the appropriate locking or conditional-rewrite protocol.
**Mutation closures are synchronous** — no `.await` inside. Never hold the lock across DHT I/O; do network work first, then mutate. `feed_sync` splits its work into two sequential `mutate_for_account` calls with the DHT writes between them, never nested.

Recompression follows the same discipline: compression (decode with the old dictionary, re-encode with the current one) runs **outside** the row lock, then the row-level write lock guards an exact-byte compare-and-swap against the row read before compression — a mismatch retries on a later cycle instead of overwriting newer data.

#### Other local records: `mutate::<T>()`

The unified mutation API in `sync/mutate.rs` handles lock acquisition, load, save, and enqueue in one call:

```rust
mutate::<ContactList, _>(&ctx, |list| {
    list.add_contact(contact);
    Ok(MutationResult::modified(()))
}).await?;
```

#### Internal/advanced: manual lock acquisition

For cases where neither API fits (e.g., multi-step merges, conditional saves):

```rust
let _guard = lock_store(key_bytes).await;
// -- critical section: load → modify → save --
let mut data = T::load_local(key_bytes).await?;
data.apply_changes(incoming);
data.save_local(key_bytes).await?;
// _guard drops here, releasing the lock
```

### ⚠ Rules

1. **Always lock before load** — acquiring the lock after loading defeats the purpose.
2. **Keep the critical section short** — no network I/O (DHT reads) inside the lock. Load from local store only.
3. **Use the right key** — conversation mutations lock the composite account+manifest storage key (`mutate_for_account`); CRDT model mutations lock the account ID. Username-keyed user-row/cleanup locks are a separate boundary, not a substitute for account/model locks.
4. **Never nest locks on the same key** — nesting different keys is safe (respect the account → conversation → row order), but nesting the *same* key deadlocks.

Inactive-account queue work deliberately outlives the active UI session. Carry
the operation's account ID into durable mutations and delivery bookkeeping;
fence late account commits against deletion/replacement under the shared
user-row lock. Only the matching current `SessionGuard` may update UI signals.

Session-scoped durable work (including conversation hydration) captures one
`UserCtx` and `SessionGuard` before asynchronous reads, then passes that same
guard through nested helpers. Keep DHT reads outside the username lock; before
each local commit, acquire `UserCtx::lock_for_account_commit(guard)` and hold it
through the store write. The helper rechecks the captured account with
`load_matching_user`, so cleanup/replacement cannot race a late write.

The shared `sync::mutate` path obtains `UserCtx::lock_for_persisted_account()` before its account-key lock, so inactive-account model work proceeds only while that exact account remains stored and cleanup serializes in the same order.
Pairing admission validates its epoch before the first durable user/AppState
write while holding both username and account locks. Once that local commit
starts, it finishes seeding the pulled identity-document history under the same
locks even if the UI epoch advances; teardown then waits for the commit and
deletes the matching account.

---

## Spawn Patterns

**File:** `src/helpers/async_utils.rs`, re-exporting Dioxus `spawn`; session-aware helpers live in `src/session/epoch.rs`.

`spawn` is Dioxus's spawn function re-exported at `crate::helpers::spawn`; it returns a `Task`. It runs on the Dioxus runtime and is safe for `GlobalSignal` access on all supported platforms, including WASM.

**Use for:** Dioxus-runtime async work, including signal writes and UI-triggered tasks.

### `spawn_blocking` — CPU-bound only

```rust
pub async fn spawn_blocking<F, R>(f: F) -> Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
```

On native: delegates to `tokio::task::spawn_blocking`. On WASM: runs inline (WASM is single-threaded).

**Use for:** Argon2 hashing, zstd compression (`ZSTD_LEVEL = 12`), dictionary training, complex math.

**Cannot access signals** — clone all data before entering the closure:

```rust
let password = password.clone();
let hash = spawn_blocking(move || argon2_hash(&password)).await?;
```

#### The wasm budget for these closures

On wasm this helper is a *label*, not an offload: there is one browser thread, so the
closure's whole duration is a stall of that thread, and the only question left is whether
it fits in a frame. Budget per closure, rounded to what the frame can absorb at 60 Hz
(16.6 ms):

| Closure work | Typical wasm cost | Verdict |
|---|---|---|
| zstd L12 compress of a ≤32 KB DHT row (`store/mode.rs`) | ~1–10 ms | fits, after the 22 → 12 change (`0187f38e`) |
| zstd decompress of the same (`codec::run_codec`) | ~1–10 ms | fits; decode cost is level-independent, so the level change did not shrink it |
| automerge load/merge/reconcile of a 10–200 KB doc (`storage/store_trait`, `chat/storage/crud`, `chat/record/**`, `chat/watcher`) | ~5–150 ms | **chunk or yield** — several per sync drain |
| Argon2id (m=19 MiB, t=2, p=1) for login/PIN/backup import | ~100–500 ms | once per action: yield around it, never in a render |
| dictionary training (`compression/trainer`) | 2–20 s, hourly | **deferred to a hidden page on web** (`lifecycle::defer_heavy_work`, retried every `DEFERRED_RESUME_MS`) |
| recompression scan (`compression/recompression`) | 0.1–3 s per wake | **deferred to a hidden page on web**, with a macrotask yielded per row |

Fixable on one thread, in order of leverage: (1) a `sleep_ms(0).await` before entering the
closure (`setTimeout(0)` — a macrotask yield, so work queued behind the current task can
paint first); (2) splitting a long closure into sub-batches that yield between them; (3)
running the two multi-second background jobs only while the page is hidden — which is
what `platform::lifecycle::defer_heavy_work()` does, and both of them
(`compression/trainer`, `compression/recompression`) now use it: they return early while
the page is visible, record nothing, and arm `config::compression::DEFERRED_RESUME_MS`
(1 minute) instead of their idle interval, so the pass lands on the first hidden moment
rather than never. A hidden page has no frame to miss, which is the whole point.

Inherent on one thread: the total CPU of a batch is unchanged by any amount of
`for_each_concurrent`/`buffer_unordered` — fan-out overlaps *awaits* (a DHT round trip
genuinely goes sum → max), never CPU. Any pass that decodes N documents still costs N
decodes on the UI thread; only the byte-level bounds in `config.rs` and the number of
sites doing it determine what that costs.

### `spawn_session_scoped` — session-scoped tasks

```rust
pub fn spawn_session_scoped<F>(future: F)
where
    F: Future<Output = ()> + 'static,
```

Subscribes directly to `SESSION_TX` at spawn time and races the task against epoch-change notifications via `tokio::select!`; when the epoch advances, the receiver fires and the task is dropped. `SessionGuard` itself contains only an `epoch: u64`; use `SessionGuard::capture().commit(...)` at critical post-await write points within the task.

**Use for:** Long-running tasks that should not survive a session change — identity syncs, feed watchers, background polling tied to a specific user session.

### Fan-out is unbounded by default

**File:** `src/helpers/concurrency.rs`

Independent work fans out with `concurrency::map_concurrent`: every item's
future is created and polled at once, so a pass costs one item's latency
instead of `ceil(n / limit)` rounds of it. Results keep input order, so callers
that zip results back against their inputs (pages, keys, route ids) need no
index bookkeeping.

This replaced the per-path semaphore budgets (`MAX_HYDRATE_CONCURRENCY`,
`SWEEP_CONCURRENCY`, `MAX_SIGNAL_FANOUT`, `RECOMPRESSION_CONCURRENCY`). Those
were throughput throttles, and on a login or sweep the throttle *was* the wall
time: every concurrent record fetch queued behind the same handful of permits.
Unbounded is the policy; measure before reintroducing a cap.

Poll those futures from their owning Dioxus task. Do not create a Tokio
`JoinSet` solely for fan-out: it imposes a native `Send` proof that the WASM
backend cannot satisfy and needlessly moves work off the Dioxus-owned future.

**Per-item timeouts, not pool sizes, are the latency bound.** A fan-out waits
for its slowest member, so each item carries its own timeout
(`route::SEND_TIMEOUT_MS`, `DHT_READ_TIMEOUT_MS`, …) rather than a shared
budget that would only queue the fast ones behind the slow one.

Bounding is **not** provided by `helpers::concurrency` on purpose. A budget
would be load-bearing only for a fan-out where *each* in-flight future holds
a large allocation, so unbounded would multiply peak heap by the item count;
when such a site appears, add a `map_concurrent_with_budget` next to
`map_concurrent` (a `Semaphore` acquired inside each task) and say why at the
call site.

**Memory bounds are byte caps, never work-unit counts** — on every platform.
A budget of 4 would mean "four futures in flight and every buffer they
produce", which is why the real memory bounds are the size caps in
`config.rs` (`MAX_MESSAGE_BYTES`, `MAX_DECOMPRESSED_BYTES`,
`PAGE_SIZE_THRESHOLD`), and why a pass that materialises a whole table first —
`recompression`'s `collect_old_rows_in_table` keeps every stale row's bytes and
then clones each one per recompress — is outside any of these bounds. When a
scan's *inputs* are unbounded, bound the inputs; a semaphore over the work units
will not help.

### Decision matrix

| Need | Tool |
|------|------|
| Async I/O, signal access, DHT calls | `spawn` |
| CPU-bound (>10ms blocking work) | `spawn_blocking` |
| Long-running task tied to current session | `spawn_session_scoped` |
| Fan-out over independent items | `concurrency::map_concurrent` (unbounded) |
| Fan-out whose futures each hold a large allocation | add a per-site `map_concurrent_with_budget` (see above); none exists today |

---

## GlobalSignal Safety

The single canonical catalog of all `GlobalSignal`s, process-local caches, and lifecycle locks lives in [README.md § Global State Catalog](../README.md#global-state-catalog). Do not maintain a second signal table here.

1. **Must run on Dioxus runtime** — only accessible from `crate::helpers::spawn`. Using `tokio::spawn` panics.
2. **Drop read guards before `.await`** — holding a `SignalReadGuard` across an await point can deadlock the reactive system.

```rust
// ✅ Correct: guard scoped and dropped before await
let username = {
    let guard = SESSION.read();
    guard.authenticated_user().map(|u| u.username().to_string())
};
some_async_call(&username).await;

// ❌ Wrong: guard held across await
let guard = SESSION.read();
let username = guard.authenticated_user().map(|u| u.username());
some_async_call(username).await; // DEADLOCK RISK
```

3. **Use `with_mut` for fire-and-forget writes** — avoids holding a write guard:

```rust
session::phase::mutate_authenticated_user(|state| *state = user);
```

---

## Static Mutexes

For data accessed from both Dioxus components and background tasks where `GlobalSignal` doesn't fit (e.g., data needed inside `tokio::spawn` on native for Veilid callbacks).

### `CONV_MANAGER`

```rust
static CONV_MANAGER: LazyLock<RwLock<ConversationManager>> =
    LazyLock::new(|| RwLock::new(ConversationManager::default()));
```

`CONV_MANAGER` in `src/chat/manager.rs` tracks conversation keys and watch lifecycle states in a `HashMap` for O(1) membership checks. Its module-level queries include `is_watched_conversation()` and `watched_conversation_keys()`; the latter returns watched and re-arm-pending keys.

### `VEILID_API` — lock-free `OnceCell`

The Veilid API handle (`src/veilid/core/mod.rs`) is a `tokio::sync::OnceCell<VeilidAPI>`, mirroring `CRYPTO_API` in `helpers/encryption.rs`. `ensure_local_api()` takes a lock-free `.get()` fast path once initialized; first call runs startup inside `get_or_try_init` (on failure the cell stays empty, so a later call retries). `veilid_shutdown()` is terminal for the process — the cell is never cleared, and there is no in-process re-init path.

### Dictionary coordination (`DICTIONARIES_READY`, `DICTIONARY_UPDATE_LOCK`)

The compression dictionary store (`src/compression/codec/registry.rs`) coordinates initialization and activation with two statics:

- `DICTIONARIES_READY` (`tokio::sync::OnceCell<()>`) — one-time bootstrap that must complete before dependent local I/O; restores the embedded structured dictionary and loads the active content identity from TableDB.
- `DICTIONARY_UPDATE_LOCK` (`Mutex<()>`) — serializes dictionary/registry activation; every add/activate/swap flows through `registry.rs` helpers, never ad-hoc registry writes.

Owned dictionary snapshots — `(DictionaryId, Arc<[u8]>)` pairs from `structured_snapshot()` — survive `.await` points and cache eviction because decode paths hold the `Arc`, not a registry guard.

### Rules

1. **Use `std::sync::RwLock`** (not `tokio::sync::RwLock`) — these locks are held for nanoseconds (lookup/insert/remove). No async work inside.
2. **Poison recovery** — always route through `recover_poison` (`helpers::store_lock`) instead of inlining `unwrap_or_else(PoisonError::into_inner)`:

```rust
let keys = chat::manager::watched_conversation_keys();
```

3. **Scope the guard** — use block expressions to drop the guard immediately:

```rust
{
    let mgr = recover_poison(CONV_MANAGER.read());
    if !mgr.is_active(&key) {
        return; // Not watching
    }
}
// Guard dropped — safe to do async work
```

---

## Cross-Platform Time

**File:** `src/helpers/async_utils.rs`

`std::time::Instant` and `SystemTime::now()` **panic on `wasm32-unknown-unknown`**.

### Safe alternatives

| Function | Returns | Implementation |
|----------|---------|----------------|
| `now_epoch_millis()` | `u64` ms since Unix epoch | `js_sys::Date::now()` on WASM, `SystemTime` on native |
| `now_epoch_secs()` | `u64` seconds since epoch | `now_epoch_millis() / 1000` |
| `sleep_ms(ms)` | async sleep | `setTimeout` on WASM, `tokio::time::sleep` on native |
| `timeout(duration, future)` | `Result<T, IoError>` | `tokio::select!` with `js_sleep` on WASM, `tokio::time::timeout` on native |
| `exponential_backoff_ms(attempt_index, base_ms, cap_ms)` | `u64` delay | one shared, saturating `base_ms * 2^attempt_index` formula; a cap, when given, bounds it. Schedules keep their base/cap/attempt indexing at the call site |

On WASM, `timeout` races the operation against `js_sleep` with `tokio::select!`. A completed operation wins when both are ready; timer-registration errors propagate as `IoError` rather than being hidden as a timeout.

### Rules

- Use `now_epoch_millis()` / `now_epoch_secs()` for all timestamps.
- Only use `Instant` or `SystemTime::now()` inside `#[cfg(not(target_arch = "wasm32"))]` blocks.
- Use `sleep_ms()` instead of `tokio::time::sleep()` for cross-platform delays.

---

## Anti-Patterns

### ❌ `tokio::spawn` for signal access

```rust
// PANICS: GlobalSignal accessed outside Dioxus runtime
tokio::spawn(async {
    session::phase::mutate_authenticated_user(|u| *u = user);
});
```

**Fix:** Use `crate::helpers::spawn`.

### ❌ Missing epoch check

```rust
spawn(async {
    let data = pull_from_dht().await; // Takes 2 seconds
    // User logged out during pull — writing corrupts new session
    session::phase::mutate_authenticated_user(|u| { /* ... */ });
});
```

**Fix:** Use `SessionGuard::capture()` + `guard.commit(...)` before writing, or use `spawn_session_scoped` for tasks that should auto-cancel on session change.

### ❌ Load-modify-save without store lock

```rust
spawn(async {
    let mut index = ConversationIndex::load_local(key).await?;
    index.add(conv);
    index.save_local(key).await?; // Another task may have saved between load and here
});
```

**Fix:** Use `mutate::<T>()` which handles locking, load, save, and enqueue atomically:

```rust
mutate::<ConversationIndex, _>(&ctx, |index| {
    index.add(conv);
    Ok(MutationResult::modified(()))
}).await?;
```

### ❌ Read guard held across await

```rust
let session = SESSION.read();
let name = session.authenticated_user().map(|u| u.username());
do_something(name).await; // Guard still held — deadlock risk
```

**Fix:** Clone the data in a scoped block, drop the guard, then await.

### ❌ Network I/O inside store lock

```rust
let _guard = lock_store(key).await;
let remote = fetch_from_dht(key).await; // Holds lock during network round-trip
let mut local = T::load_local(key).await?;
local.merge(remote);
local.save_local(key).await?;
```

**Fix:** Do network I/O *before* acquiring the lock. The lock protects local state only.

### ❌ `Instant::now()` in cross-platform code

```rust
let start = std::time::Instant::now(); // PANICS on WASM
```

**Fix:** Use `now_epoch_millis()` or gate with `#[cfg(not(target_arch = "wasm32"))]`.

---

## Timer Discipline

**Files:** `src/user/inspect_poll.rs`, `src/compression/{recompression,trainer}.rs`,
`src/sync/queue/mod.rs`, `src/config.rs`

Wait for something you can observe, not for a clock. The app's convention is a
`select!` between the event that ends the wait — `NETWORK_CHANGED`,
`APP_ACTIVITY_CHANGED`, `RECOMPRESS_NOTIFY`, `TRAIN_NOTIFY`,
`SYNC_QUEUE.wake_notify` — and a timer that is only a backstop for an event that
was *lost*. Copy that shape rather than adding a new poll loop.

A clock site is legitimate when no observable event can exist:

- **Retry/backoff against the DHT.** These exist so one failure cannot become a
  retry storm. Every one is a named constant — most in `config`, the rest beside
  the loop they guard (`VALUE_RETRY_DELAY_MS`, `OPEN_RETRY_DELAY_MS`,
  `PUBLISH_VERIFY_BACKOFF_MS`, `PUBLISH_RETRY_BASE_MS`,
  `RENEWAL_DEDUP_INTERVAL_MS`, `BEACON_WRITE_RETRY_WAITS_MS`,
  `TRAIN_RETRY_FLOOR_MS`) — never inline a duration at the call site. The delay
  itself comes from
  `helpers::async_utils::exponential_backoff_ms`; do not duplicate its
  shift/multiply formula.
- **A settling window for state the app cannot observe.** DHT propagation is the
  standing case: the e2e pairing retry waits because no event proves the
  anchor's identity record has replicated.
- **Presentation linger** — leaving a completed state on screen briefly.
- **Device frame polling** where the platform offers no push (QR capture).

### ⚠ Rules

1. Name the duration where it is used, in `config`, like its siblings. A bare
   literal in a call site is the thing this section exists to prevent.
2. Keep the generation / `pairing_id` / `guard.is_current()` re-check that makes
   a stale timer harmless when it finally fires.
3. Never replace a loud timeout with an unbounded wait: a wait that cannot fail
   is worse than a delay. Where a wait has a soft budget, catch it explicitly
   and say so at the call site.
4. `spawn_blocking` is for CPU-bound work; see [Spawn Patterns](#spawn-patterns).
   File system work on an async path (a directory walk, a recursive delete)
   belongs there too.
5. `clippy::large_futures` is enforced. When a future that crosses a hot-path
   boundary is large, box it at that boundary (or `Box::pin` its await site) —
   do not add a crate-wide `large_futures` allowance.
