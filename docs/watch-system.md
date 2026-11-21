# DHT Watch System — Architecture

**Owns:** `src/chat/watcher/`, `src/veilid/change_dispatcher.rs`, `src/user/watcher/`, `src/user/identity_watch.rs`

## Overview

The watch system subscribes to Veilid DHT `ValueChange` notifications for conversation SMPL records so remote changes merge push-first, with sibling-device nudges and an adaptive inspect poll as backstops. Watches are **session-owned**: every conversation in the `ConversationIndex` is watched from login to logout (capped at `config::chat::MAX_CONVERSATION_WATCHES`, most recently active first), regardless of which screen is open — a message arriving in a background conversation lands as a native `ValueChanged`, updates the local store, and bumps the index preview/unread. A single watch covers the whole SMPL record, and local processing uses one bounded change-dispatch stage:

1. **Update ingress** — `UpdateIngress` (`veilid/core/updates.rs`) forwards each `ValueChange` directly to the change dispatcher.
2. **Change dispatcher** — `DirtyChangeDispatcher` (`veilid/change_dispatcher.rs`) coalesces dirty keys in a bounded `CoalescedState` (capacity `CHANNEL_CAPACITY = 256`). New distinct keys that arrive at capacity cannot be queued individually; one overflow marker schedules forced recovery. If more keys overflow during a sweep, one follow-up sweep is queued. Each coalesced job is dispatched on its own task with no permit pool — the coalescing queue itself stays bounded, only the handler fan-out is unbounded.

The result is eventual recovery without blocking Veilid callback threads or introducing unbounded buffers.

## Tracking State

`CONV_MANAGER` (`src/chat/manager.rs`) owns a process-local `ConversationManager`:

| Field | Type | Purpose |
|---|---|---|
| `active` | `HashMap<Arc<str>, WatchState>` | Per-conversation watch lifecycle (`Opening`, `Open`, `NeedsRearm`, `Closing`) |
| `open_conversation` | `Option<Arc<str>>` | The conversation currently on screen (presence-signal gating only) |

Watch lifetime is decoupled from the UI:

- **Login** — `chat/record/catchup.rs::arm_and_catch_up()` (spawned from `session/phase.rs::complete_admission` after identity resync) arms a watch for every indexed conversation via idempotent `ensure_watched()`, cold-hydrates conversations the local store doesn't know yet, then runs the catch-up sweep (below).
- **Index change** — when an identity resync pulls a changed `ConversationIndex` (`user/watcher/`), `reconcile_watches()` arms new keys, hydrates unknown ones, and unwatches keys that vanished (conversation deleted on any device).
- **UI open/close** — `src/chat/hooks/mod.rs` only calls `set_open_conversation()` / `clear_open_conversation()` plus an opportunistic `ensure_watched()` (repair path if the login-time arm failed). Closing a conversation never cancels its watch.
- **Signal promotion** — a verified `PeerSignal::NewData` also fires `ensure_watched()`, covering just-created conversations whose index entry hasn't synced yet.
- **Logout** — `cancel_all_watches()` drains the registry and unpins every record.
- **Watch expiry** — `count == 0` triggers renewal. `watch_record()` surfaces Veilid's `Ok(false)` as `WatchError::Inactive`; conversation watches transition to `NeedsRearm`, re-open once, and keep the key tracked with one bounded delayed re-arm if setup still fails.

## Login Catch-Up (subscribe-then-snapshot)

Watches only deliver changes that happen while they exist; writes made while this device was offline need a one-shot sweep. `arm_and_catch_up()` arms all watches **first**, then for each locally-known conversation runs one `inspect_record(…, DHTReportScope::SyncGet)` over `full_conversation_subkeys()` and feeds `newer_online_subkeys()` — subkeys whose network seq is ahead of the local cache — into the normal `handle_conversation_change()` pipeline, which force-reads exactly those subkeys. Quiescent conversations cost one inspect RPC and zero reads; anything written during the sweep is caught by the already-armed watches.

## SMPL Coverage

A conversation watch always targets the full SMPL record (`watch_dht_values(key, None)`), including **our own member subkeys**: all devices on one account share a single member slot (the writer is the identity keypair), so another device's feed writes arrive as ordinary member-page changes and merge into the local store — this is what keeps a two-device account's sent messages in sync. Veilid does not echo a node's own writes back to it, so watching our own subkeys costs nothing locally. `None` also automatically covers subkeys of members added to the roster later:

- `0` — `ConversationManifest`
- `1` — `ConversationRoster`
- `2` — `ConversationKeyRing`
- `3 + (member * 2)` — `ParticipantFeedMeta`
- `3 + (member * 2) + 1` — member head `MessagePage`

Archive pages live in chained DFLT records, so the live watch only sees the archive pointer metadata (`ParticipantFeedMeta`). Archive bodies are backfilled lazily.

## Conversation Change Handling

`src/chat/watcher/mod.rs::handle_conversation_change()` now handles all conversation-relevant subkeys incrementally:

| Change | Live action |
|---|---|
| Manifest | Reload only manifest and save locally |
| Roster | Reload roster (and manifest only if requested), save locally |
| Key ring | Reload key ring and save locally |
| Feed meta | Reload only that member’s `ParticipantFeedMeta`, update `my_feed_meta` when the slot is ours, and decide whether archive backfill is needed |
| Head page | Reload only the touched member page and merge messages locally |

### Watch receiver snapshots (no `watch::Ref` across awaits)

Watch workers snapshot a receiver's value into an owned local before async handling: `spawn_watch_worker` clones `borrow_and_update()` into a separate statement, then awaits the handler (`on_update(update).await`) separately. A `watch::Ref` borrows the guarded cell and must never be held across `.await` — handlers receive the cloned value, so long DHT reads cannot stall the watch channel.

### Member-page merge

The changed page subkey identifies a roster slot; the handler resolves that slot and only merges pages for active members. It normalizes each remote message's `sender_username` to the roster member's username instead of trusting the payload's display field.

Pages are merged through the shared `union_messages_in_place_with_seen()` helper using `message_dedup_key()` derived from `msg_id`; there is no legacy `(seq, identity_key)` dedup fallback. The message payload has no per-message identity key.

Preview and `last_message_at` always advance, but unread does not accrue for the conversation currently on screen: `update_conversation_preview_for_account()` skips the unread increment when `manager::is_open_conversation()` reports it is open.

### Lazy archive backfill

A feed-meta refresh does **not** immediately load archive chains on every change. Instead, the watcher checks whether local messages show a gap relative to `head_page_start_seq` / `archive_key`. Only then does it spawn a background `load_archive_chain()` for that member and merge the recovered pages.

Cold hydration and archive backfill fan out with **unbounded** concurrency (`helpers::concurrency::map_concurrent`, see [docs/concurrency.md](concurrency.md)): member head-page pulls all start together, archive-chain loads do the same per member, and results are merged back in deterministic member order before the final local-message sort. There is no semaphore budget left to share; the bound on a run is the record byte caps in `config.rs` plus each fetch's own timeout, not a pool size.

### Head-first cold open

Opening a conversation from a cold local cache now uses `hydrate_conversation_heads()` first:

1. load key ring / manifest / roster / current head pages
2. save head state locally so the conversation can render immediately
3. start archive-chain backfill in a background task
4. touch `CONVERSATION_META.last_feed_update` when backfill lands so the open timeline reloads automatically

The conversation UI shows inline loading state while head hydration is in flight and a separate “loading older messages” state while archive backfill continues.

## Overflow Recovery

When the bounded dirty-key map is full, newly distinct key hints are dropped and one overflow marker requests forced recovery:

1. Resync the authenticated identity record.
2. Load the local `ConversationIndex` and iterate its conversations.
3. For each conversation, call `handle_conversation_change()` with the full local subkey range, dispatching conversations unbounded via `helpers::concurrency::map_concurrent` — each conversation serialises on its own lifecycle lock, so the fan-out adds no shared contention.

The sweep runs outside Veilid callbacks and holds no overflow-event queue. If another distinct key overflows while a recovery sweep is in flight, the coalescer queues exactly one follow-up sweep.

## Session Currency Rules

The watch path re-checks session currency after awaited DHT work and before local-store / signal writes:

- watcher owner/member updates
- invite acceptance DHT pulls
- cold hydration saves
- watch start/stop orchestration

`cancel_all_watches()` still performs explicit logout cleanup, but stale UI lifecycle work is filtered through `SessionGuard`.

## Identity Record Watch (session-pinned)

Besides conversation records, the authenticated user's **identity record** carries one whole-record watch for the entire session, owned by `user/identity_watch.rs`:

- **Pinning**: `store::pin_record` keeps the identity record open with the session writer keypair until logout. Scoped `with_record*` operations borrow the pinned open, so sync-queue writes and the watch coexist without open/close races (Veilid watches die when their record closes).
- **Lifecycle**: started after admission (`session/phase.rs`) and when the identity record is first created; re-established on network reattach (`veilid/core/updates.rs`) and on watch expiry; stopped on logout/account deletion under a `begin_teardown` guard held across epoch-advance → stop → session-clear, so a concurrent (re)start can never re-pin a logged-out identity.
- **Subkey dispatch** (`user/watcher/::handle_identity_change`) is **scoped** (`ResyncScope`): profile/security/app-state hints → core-subkey resync only; a pointers (4) hint → core + child records + child-watch re-arm (child keys may have appeared/migrated); dict subkeys → dictionary-only sync (run even when core subkeys changed in the same coalesced event); routes (8) → evict that identity's cached imported routes; pairing beacon (9) → push to `REMOTE_BEACON` for live pairing status (no data resync). `count == 0` re-arms the watch; with an empty subkey set (watch death or unknown scope) it triggers a full resync — core + children + dict check.
- **Value attribution**: Veilid attaches the delivered value to the *first* changed subkey only — and commits it to the local record store only for single-subkey changes. Coalesced multi-subkey hints are value-less and do **not** update the local cache, so handlers must force-read (network) the subkeys they act on.
- **Pull freshness**: `user/record/::sync_with_policy` runs one `SyncGet` inspect over the core subkeys before its read rounds and force-reads exactly the `newer_online_subkeys()`; unchanged subkeys stay local cache reads. If the inspect fails, every read is forced — the inspect is an optimization, never a correctness gate. Child pulls (`pull_and_merge`) always read forced.

Identity/child watches deliver profile, app-state, pairing, and dictionary changes push-style in the normal case. A separate session-scoped Tier-3 inspect poll in `src/user/inspect_poll.rs` compares network sequence numbers for the identity and watched child records to recover missed watch/nudge events; it runs every 15 seconds while push paths look unhealthy and uses a 60-second cadence after three ticks without a detected missed change (failed inspections also decay the fast cadence); a pending wait is cut short by the `sync::APP_ACTIVITY_CHANGED` foreground edge, so a tick a backgrounded or throttled platform delayed lands as soon as the app is active again. Veilid also renews watches and reconciles them on reattach.

## Child Record Watches (session-pinned)

`src/user/child_watch.rs::sync()` watches each existing identity child pointer (TOTP, contacts, and conversation index). Pointer movement, removal, or invalid encoding removes the old pin/watch before reconciliation. Its `LIFECYCLE` mutex serializes reconciliation with logout/account teardown; teardown holds the guard across epoch advance, unpin, and session clear. Keys enter the tracked/inspect set only after pin succeeds; failed pins are retried on network reattach because Veilid cannot recover a watch it never received.


## Direct Peer Signaling Integration

`PeerSignal::NewData` remains the low-latency accelerator for member feed pulls, but it is now sender-proved before use:

- signal carries the sender's claimed identity plus a signature proof from the sender's identity-record keypair
- dispatch derives sender identity from that proof before roster checks and requires it to match the on-wire sender identity
- `member_subkey` must match the sender’s roster slot
- only then does it trigger a targeted `handle_conversation_change()` pull

Signal gating is split by kind: presence-style signals (typing / stop-typing) require `is_open_conversation()`; `NewData` triggers a member pull for a known conversation whether open or not. Same-account `SyncNudge` requests identity, child-record, or dictionary pulls, while invites use their own persistence/ack flow. Tag `0x03` is reserved; `ReadUpTo` is not a current signal.

DHT watches remain the durable fallback when `app_message` delivery is missed.

## Key Files

| File | Responsibility |
|---|---|
| `src/user/identity_watch.rs` | Session-owned identity record pin + whole-record watch lifecycle (start/stop/reestablish under one lifecycle lock) |
| `src/user/inspect_poll.rs` | Adaptive Tier-3 identity/child-record inspect poll for missed push updates (15s fast / 60s slow) |
| `src/user/watcher/` | Identity `ValueChange` subkey dispatch (profile/dict/routes/pairing-beacon) |
| `src/chat/manager.rs` | Session-owned watch registry (`ensure_watched`/`unwatch_conversation`/`cancel_all_watches`), open-conversation tracking, watch renewal |
| `src/chat/record/catchup.rs` | Login watch arming + catch-up sweep (`arm_and_catch_up`), index-driven watch reconcile (`reconcile_watches`) |
| `src/chat/hooks/mod.rs` | Marks the on-screen conversation open/closed; opportunistic `ensure_watched` repair on open |
| `src/chat/watcher/mod.rs` | Incremental manifest/roster/key-ring/feed-meta/page pulls, sender validation, lazy archive backfill, `CONVERSATION_META` feed-update touch |
| `src/chat/watcher/updates.rs` | Sub-module: owner subkey change loading (manifest, roster, key-ring updates) |
| `src/chat/watcher/member.rs` | Sub-module: combined owner + member feed-meta/head-page mutation and archive backfill triggers |
| `src/chat/record/hydration.rs` | Single-open head hydration, reusable background archive backfill with unbounded fan-out, malformed-key progress handling |
| `src/veilid/change_dispatcher.rs` | `DirtyChangeDispatcher` + `CoalescedState` / `CoalescedJob`: bounded dirty-key coalescer and overflow-triggered conversation resync sweep |
