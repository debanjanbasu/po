# Peer Signaling Architecture

**Owns:** `src/chat/signal/`, `src/chat/hooks/`, `src/chat/invite/`

Ephemeral peer-to-peer signals are sent through Veilid `app_message` for low-latency delivery (~50 ms). They are **accelerators, not the source of truth**: durable state still flows through the local-first sync pipeline and DHT watchers.

Because Veilid private routes keep sender identity anonymous when sender privacy is enabled, conversation-scoped signals cannot trust network-level source metadata. Instead, the app appends an in-band sender proof signed by the sender's identity-record keypair.

## Key Files

| File | Purpose |
|------|---------|
| `src/chat/signal/wire.rs` | Signal variants, stable tag assignments, binary encoding/decoding, sender-proof format |
| `src/chat/signal/dispatch.rs` | Incoming signal parsing, proof verification, and handler routing |
| `src/chat/signal/nudge.rs` + `src/sync/queue/mod.rs` | Same-account sibling-device nudge send path and post-publish scope aggregation |
| `src/user/watcher/resync.rs` | Scope-selected identity/child pulls after a nudge |
| `src/chat/signal/routes/routes_send.rs` | Route publish/resolve/cache, prepared-payload fanout, and per-identity route expiry |
| `src/chat/hooks/send.rs` + `src/chat/hooks/mod.rs` | New-data and typing/stop-typing send-side integration |
| `src/chat/invite/` | Durable invite persistence/accept flow, proof-verified invite-ack handling, and sender-signed `NewData` fan-out |

---

## Signal Model

```rust
pub struct SenderProof {
    pub sender_public_key: PublicKey,
    pub signature: Signature,
}

pub enum SignalPayload {
    Typing {
        conv_key: RecordKey,
        sender_identity: RecordKey,
    },
    StopTyping {
        conv_key: RecordKey,
        sender_identity: RecordKey,
    },
    NewData {
        conv_key: RecordKey,
        sender_identity: RecordKey,
        member_subkey: u32,
    },
    ConversationInvite {
        manifest_key: RecordKey,
        inviter_key: RecordKey,
    },
    ConversationInviteAck {
        manifest_key: RecordKey,
        inviter_key: RecordKey,
        invitee_key: RecordKey,
    },
    SyncNudge {
        identity_key: RecordKey,
        scope: u8,
        device_id: Arc<str>,
    },
}

pub struct PeerSignal {
    pub payload: SignalPayload,
    pub proof: Option<SenderProof>,
}
```

### Sender-binding rules

Conversation-member and invite signals must prove the sender before mutating local state:

| Signal | Binding requirement |
|---|---|
| `Typing` / `StopTyping` | valid `proof`, derived sender identity must match on-wire `sender_identity`, and that member must be active in the local roster |
| `NewData` | same sender-proof + roster-membership check **plus** `member_subkey` must match the sender's roster slot |
| `ConversationInvite` | valid `proof`, derived sender identity must match `inviter_key`, then the invite is persisted account-scoped before pending/auto-accept handling |
| `ConversationInviteAck` | valid `proof`, derived sender identity must match `invitee_key`, and the ack only completes the matching outbound `ConvInviteSignal` work for that `(manifest_key, inviter_key, invitee_key)` |

Dispatch derives the trusted identity from `proof.sender_public_key` by recomputing the sender's default DFLT(16) identity record key, then checks that the on-wire sender identity matches.

`SyncNudge` is identity-scoped rather than conversation-scoped. Its proof must derive the same identity as `identity_key`; the receiver requires that key to be its own authenticated identity and does not perform conversation-roster checks.

---

## Wire Format

Each signal is a compact binary blob: `[1-byte tag][fixed-width keys][payload]`. Every length-prefixed field in these layouts (the sender-proof public key and signature, the `SyncNudge` device id) is written and read through the single bounds-checked framing module `helpers::framing` (`push_u16_len_prefixed` / `read_u16_len_prefixed`) — there is no hand-rolled prefix code in `wire.rs`.

### Tag bytes

| Tag | Variant |
|-----|---------|
| `0x01` | Typing |
| `0x02` | StopTyping |
| `0x03` | Reserved (removed `ReadUpTo`; do not reuse or renumber later tags) |
| `0x04` | NewData |
| `0x05` | ConversationInvite |
| `0x06` | ConversationInviteAck |
| `0x07` | SyncNudge |

### `RecordKey` encoding

```text
[4 bytes: kind][32 bytes: key] = 36 bytes per RecordKey
```

### Sender proof encoding

```text
sender_proof
  [public_key_kind: 4]
  [public_key_len: u16 LE][public_key]
  [signature_len: u16 LE][signature]
```

### Layouts

```text
Typing / StopTyping
  [tag][conv_key][sender_identity][sender_proof]

NewData
  [tag][conv_key][member_subkey: u32 LE][sender_identity][sender_proof]

ConversationInvite
  [tag][manifest_key][inviter_key][sender_proof]

ConversationInviteAck
  [tag][manifest_key][inviter_key][invitee_key][sender_proof]

SyncNudge (tag 0x07)
  [tag][identity_key][scope: u8][device_id_len: u16 LE][device_id: UTF-8][sender_proof]

```

`SyncNudge`'s `identity_key` uses the 36-byte `RecordKey` encoding above. The sender proof is the same length-prefixed public-key/signature tail used by every signal, encoded/decoded via `helpers::framing`.

---

## Sending

Every send enters through `save_and_queue_message` (`src/chat/hooks/send.rs`): the composer, and the harness-only `send_from_deep_link` behind `po://send?…` (`platform::deep_link`, refused unless `config::e2e::enabled()`), which resolves the contact and calls the same function with no conversation view open. The seam adds no signal of its own — a message sent that way fans out exactly the signals below.

### Typing

`chat/hooks/mod.rs` emits `Typing` / `StopTyping` with the local identity key via its in-module typing flow helpers. `prepare_signal_bytes()` attaches the sender proof once, and `send_prepared_signal_excluding()` reuses that payload across the bounded route fan-out. Delivery is still best-effort and intentionally lossy.


### New data nudges

After a successful local save + queue enqueue, send-side code fans out:

```rust
PeerSignal::NewData {
    conv_key,
    sender_identity: my_identity_key,
    member_subkey: my_feed_subkey,
    proof: None,
}
```

Receiving `NewData` automatically clears any active typing indicator for that sender before triggering the targeted member pull. If that pull observes no change for the named member subkey — the signal outran the sender's DHT write reaching the nodes this device reads from, the same race `SyncNudge` covers — it re-pulls once after `RETRY_DELAY_MS` (2 seconds), and only while the session is still current.
Invite delivery now uses an explicit ack loop:

- bootstrap (`enqueue_invite_retry`) signs `ConversationInvite` with the inviter's identity-record keypair when enqueuing (`to_signed_bytes`) and enqueues a durable `ConvInviteSignal` op addressed to **two typed keys**: the invitee's record key (`target_key`) and the conversation's manifest key (`target_subkey`). They are separate fields, not one joined string — a Veilid record key already contains colons (`"<key>:<encryption_key>"`), so a `"{peer}:{manifest}"` composite could only be split at the wrong place (it truncated the peer to `"VLD0"`, and every invite went to nobody)
- the op keeps retrying with backoff until a `ConversationInviteAck` arrives from that invitee
- the receiver sends `ConversationInviteAck` after the invite is durably persisted, so retries stop on receipt even before manual acceptance
- a *delivered*-but-unacked invite is not a delivery failure: its re-sends are capped by their own wall-clock budget (`config::sync::INVITE_ACK_WAIT_MS`, 24h from enqueue) instead of the 12-attempt delivery budget, so a peer that is online but slow to ack never gets a "Sync gave up after 12 attempts" dead-letter. The lane still ends — after that wall-clock budget it dead-letters for manual recovery, and the attempt counter keeps advancing so the queue's retry accounting stays finite
- the invite's retry schedule is **10s, 20s, 40s, 80s, … capped at 2h** (base 5s). Delivery only lands while the peer is online, so the early attempts stay close together: a peer that is momentarily unreachable — a network flap, a route that has not settled — gets the invite within the same interaction rather than after the old two-minute first retry

Invite acceptance still hydrates manifest/roster/key-ring plus the current member head pages before any `ConvFeedSync` enqueue can publish local defaults, then sends sender-signed `NewData` nudges after local state is saved. Archive history backfills in a session-scoped background task so accept is not O(history).

Signals still fan out to **all** resolved device routes for the peer identity, and the route sends run unbounded via `helpers::concurrency::map_concurrent` (see [docs/concurrency.md](concurrency.md)), reusing one prepared payload buffer per fan-out. Route-cache expiry is lazy/per-identity instead of a full-map prune on every send. If all routes fail, route cache eviction forces the next send to re-resolve from DHT; for **durable** signals (`NewData`, invite and invite-ack) that send then re-resolves once and re-sends once, because a peer that restarted without republishing keeps being re-imported from a fresh-but-dead blob until its cache entry expires. `Typing`/`StopTyping` stay single-shot: a replay would resurrect an indicator the peer already cleared. `SyncNudge` is also single-shot — the durable paths (watches, inspect poll) already cover a lost nudge.

### Route publication

A device publishes its private route blob to subkey 8 of its own identity record. The publish runs on session start and on each DHT-readiness edge, and retries with backoff (2s → 4s → 8s → 16s → 32s, six attempts).

**"No identity yet" is a retryable outcome, not success.** At session start the identity record is created by an async `CreateIdentity` op, so a publish that runs before it lands has nowhere to write. Treating that as success ended the retry loop, and the device stayed unreachable to peers until the 45 s route heartbeat happened to publish — long enough for a peer's first invite to find an empty route subkey and be deferred to the (2 h-capped) invite backoff.

### Same-account sync nudges

After a sync-queue lane publishes identity-scoped changes, `src/sync/queue/mod.rs` combines the affected scopes and calls `nudge_sibling_devices()` in `src/chat/signal/nudge.rs`. The signed tag-`0x07` signal targets the account's `identity_key`, includes the sending `device_id`, and fans out to sibling routes while excluding the local device. Conversation-record operations use `NewData`/watches instead.

The `scope: u8` mask is defined in `config::nudge` (`src/config.rs`): `0x01` core identity, `0x02` TOTP, `0x04` contacts, `0x08` conversation index, `0x10` dictionary, and `0x20` full resync. `SCOPE_FULL` requests a full identity resync.

`SCOPE_CONVERSATIONS` pulls the conversation index and then sweeps **only the conversations whose index `last_message_at` advanced**, one `SyncGet` each. The index pull proves a message landed somewhere; the message itself lives in the conversation record, whose own watch can be lost, and the sweeps that exist for that record (login, watch reconcile, watch re-arm) only run at their own moments — a conversation that is already watched and already known is otherwise never re-inspected. Scoping to the advanced entries keeps the cost at one inspect per message rather than one per watched conversation.

---

## Receiving

`handle_app_message()` in `src/chat/signal/dispatch.rs` parses a signal and routes it to its handler. Conversation signals verify the shared sender proof against the claimed identity, then validate active roster membership (and the `NewData` member slot) before acting.

For `SyncNudge`, the path is:

1. `src/chat/signal/wire.rs` parses tag `0x07`, `identity_key`, `scope`, UTF-8 `device_id`, and the required shared sender-proof tail; reserved tag `0x03` is rejected.
2. `src/chat/signal/dispatch.rs::handle_sync_nudge_signal()` requires the target key to match the current account identity and ignores a nudge from this device.
3. Verify the signature and require the public-key-derived sender identity to match `identity_key`; discard the work if the session changed.
4. Call `src/user/watcher/resync.rs::handle_sync_nudge(scope)` to pull the selected scopes. If no change is observed, it retries once after `RETRY_DELAY_MS` (2 seconds).

### Resulting behavior

| Variant | Post-verification action |
|---|---|
| `Typing` | Update `CONVERSATION_META.typing_at` and add the proved peer to `typing_peers`, only while the conversation is open |
| `StopTyping` | Remove that proved peer from `typing_peers`; clear `typing_at` when set empties |
| `NewData` | Trigger a targeted single-member watcher pull; re-pull once after 2 seconds if that pull observed no change for the named subkey |
| `SyncNudge` | Run the requested identity/child/dictionary pulls; retry once after 2 seconds if no change is found |
| `ConversationInvite` | Verify `inviter_key`, persist the invite, ack receipt, then auto-accept known contacts or leave a durable pending invite for manual retry |
| `ConversationInviteAck` | Verify `invitee_key`, then complete the matching outbound invite-delivery queue work |

---

## Durability reminder

Signals are not authoritative storage:

- typing is purely ephemeral
- `NewData` is a fast nudge
- same-account `SyncNudge` is a fast identity/child-record pull accelerator
- invites use a sender-proved `ConversationInvite` + `ConversationInviteAck` loop for durable-enough delivery, but membership/message truth still comes from DHT hydration
- message bodies, feed metadata, roster changes, and archive history still recover through DHT sync + watchers

If a signal is dropped or rejected, correctness falls back to the durable path.
