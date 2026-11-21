// oxlint-disable max-lines -- one file per scenario: this flow (pair, second account, invite, message both ways) only reads top to bottom as one piece
// S17 — pair a device, then message it: the native path into messaging.
//
// S3 cannot run on a native surface: it needs `signIn` on BOTH peers, and a
// native surface has no typing channel. Pairing is the way in — S1's joiner
// ends up signed in with an account without a keystroke — and this scenario
// then puts that account into a real conversation, asserting S3's markers
// across the device boundary: `message_sent`, `message_delivered` and
// `queue_drained` on the sender, `message_received` naming the same `msg_id`
// on the paired device, and the rendered bubble on the sender's side.
//
// Layout: the FIRST surface hosts the anchor (peer a) and the counterpart
// (peer b) — two isolated contexts, exactly the two accounts S2/S3 talk
// through — and the joiner is the last peer: a third context on that same
// surface (web-only runs), or the device surface when one is listed. Two
// devices of ONE account cannot message each other — the app refuses a
// self-contact (`ChatInputError::SelfContact`,
// `src/chat/session_flow/mod.rs`) — so the paired account needs a counterpart
// account to talk to, and switching the anchor peer's own account over (log
// out, sign in again) would put the scenario's timing on the app's logout.
//
// The paired device SENDS the invite, not the counterpart: an invite from an
// unknown sender parks until it is accepted by hand
// (`src/chat/invite/receiver.rs`), and a device with no DOM cannot press
// Accept — the adder's own path needs no acceptance click, and a contact deep
// link is the only way a typing-less device can ADD one (the send action
// below writes into a conversation that exists; it cannot create one).
//
// The return direction needs a send channel the paired device may not have. A
// DOM joiner types into the composer; a native one drives `po://send?…`, the
// harness-only deep-link action that runs the app's ordinary send path from a
// launch URL (`src/platform/deep_link`, gated on `config::e2e::enabled()`, and
// `chat::hooks::send_from_deep_link` behind it). A surface with neither
// channel records a SKIP rather than passing quietly.

import { waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import {
  BUDGET,
  PASSWORD,
  assertDhtReady,
  contactLink,
  countMarker,
  pairSecondDevice,
  recordIdentity,
  recordObservation,
  sendLink,
  skip,
  waitMarkerCount,
} from './support.ts';

export const name = 'S17';
export const description =
  'Pair a joiner, then message it both ways: the paired device joins a real conversation and takes chat traffic';

/** The counterpart account: a second account on the anchor peer's surface. */
const COUNTERPART_USERNAME = 'e2e-s17-counterpart';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  if (surfaces.length === 0) throw new Error('S17 needs at least one surface');
  // The DOM surface hosts the anchor and the counterpart; the joiner is the
  // last peer of the last surface (a third context there, or the device).
  const domSurface = surfaces[0];
  const joinerSurface = surfaces[surfaces.length - 1];
  const anchor = { surface: domSurface, peer: domSurface.peerIds[0] };
  const counterpart = { surface: domSurface, peer: domSurface.peerIds[1] };
  const joined = {
    surface: joinerSurface,
    peer: joinerSurface.peerIds[joinerSurface.peerIds.length - 1],
  };
  if (!counterpart.peer || counterpart.peer === joined.peer) {
    throw new Error(
      `S17 needs the first surface to host two peers and the joiner to be a third ` +
        `(${domSurface.name} hosts ${domSurface.peerIds.join(',')}, ` +
        `${joinerSurface.name} hosts ${joinerSurface.peerIds.join(',')}) — ` +
        'run it with --surfaces web or web,<device>',
    );
  }

  // The anchor peer must be able to sign in, share, and later hold the
  // counterpart account's side of the conversation.
  const missing: string[] = [];
  if (!anchor.surface.signIn || !anchor.surface.openShareScreen || !anchor.surface.readShare) {
    missing.push(`${anchor.surface.name}: signIn/openShareScreen/readShare`);
  }
  if (!anchor.surface.identity) missing.push(`${anchor.surface.name}: identity`);
  if (!anchor.surface.acceptInvite) missing.push(`${anchor.surface.name}: acceptInvite`);
  if (
    !anchor.surface.openConversation ||
    !anchor.surface.sendMessage ||
    !anchor.surface.readMessages
  ) {
    missing.push(`${anchor.surface.name}: openConversation/sendMessage/readMessages`);
  }
  if (missing.length > 0) {
    skip(ctx, anchor, 'pair-then-message capabilities', missing.join('; '));
    return;
  }

  // ── 1. Pair: the anchor's account lands on the joiner (S1's flow) ───────
  const paired = await pairSecondDevice(ctx, anchor, joined, 'e2e-s17-account', BUDGET);
  if (!paired.ok) {
    skip(ctx, joined, 'join an account', paired.reason);
    return;
  }
  // The joiner RAN the joining path, and that is the only path that emits
  // `dht_ready` (support.ts::assertDhtReady) — the gate is asserted here, on
  // the device that joined, and nowhere else in this scenario.
  await assertDhtReady(ctx, joined, BUDGET);
  await waitForSignal(
    ctx,
    joined.surface,
    joined.peer,
    { label: 'pairing complete (joined device)', marker: 'paired' },
    BUDGET,
  );
  const account = paired.anchorIdentity;
  if (!account) {
    skip(ctx, anchor, 'identity', `${anchor.surface.name} reported no account identity to address`);
    return;
  }
  ctx.record({
    surface: joined.surface.name,
    peer: joined.peer,
    kind: 'step',
    label: 'joined device holds the paired account',
    line: `account username=${account.username} (from pairing, not from a sign-in)`,
  });

  // ── 2. The counterpart account ──────────────────────────────────────────
  // A plain sign-in on its own context: no account switch, so its identity
  // marker is the first one this peer ever emitted and `recordIdentity` cannot
  // read a stale line.
  if (!counterpart.surface.signIn || !counterpart.surface.identity) {
    skip(
      ctx,
      counterpart,
      'counterpart account',
      `${counterpart.surface.name} cannot sign in as the second account`,
    );
    return;
  }
  await counterpart.surface.signIn?.(counterpart.peer, COUNTERPART_USERNAME, PASSWORD, BUDGET);
  const counterpartIdentity = await recordIdentity(ctx, counterpart);
  if (!counterpartIdentity) {
    skip(
      ctx,
      counterpart,
      'identity',
      `${counterpart.surface.name} reported no username/dht_key for the counterpart`,
    );
    return;
  }

  // ── 3. The joined device adds the counterpart ───────────────────────────
  // A contact deep link is the whole outbound chat capability a device with no
  // typing channel has; on web `openUrl` routes a contact link to the app's own
  // paste path.
  await joined.surface.openUrl(
    joined.peer,
    contactLink(counterpartIdentity.username, counterpartIdentity.dhtKey),
  );
  await waitForSignal(
    ctx,
    joined.surface,
    joined.peer,
    { label: 'invite sent (joined device)', marker: 'invite_sent' },
    BUDGET,
  );

  // ── 4. The counterpart accepts; both sides reach Active ─────────────────
  await counterpart.surface.acceptInvite?.(counterpart.peer, BUDGET);
  const counterpartReceived = await waitForSignal(
    ctx,
    counterpart.surface,
    counterpart.peer,
    { label: 'invite received (counterpart)', marker: 'invite_received' },
    BUDGET,
  );
  const counterpartActive = await waitForSignal(
    ctx,
    counterpart.surface,
    counterpart.peer,
    { label: 'conversation active (counterpart)', marker: 'conversation_active' },
    BUDGET,
  );
  const joinedActive = await waitForSignal(
    ctx,
    joined.surface,
    joined.peer,
    { label: 'conversation active (joined device)', marker: 'conversation_active' },
    BUDGET,
  );
  const conversation = joinedActive.fields['conversation'];
  if (!conversation) {
    throw new Error(
      `event=conversation_active has no conversation field: ${ctx.mask(joinedActive.line)}`,
    );
  }
  if (counterpartActive.fields['conversation'] !== conversation) {
    throw new Error(
      `the two sides disagree about the conversation\n` +
        `  joined device: ${ctx.mask(conversation)}\n` +
        `  counterpart:   ${ctx.mask(counterpartActive.fields['conversation'] ?? '(none)')}`,
    );
  }
  if (counterpartReceived.fields['conversation'] !== conversation) {
    throw new Error(
      `the counterpart's invite names another conversation: ${ctx.mask(counterpartReceived.fields['conversation'] ?? '(none)')}`,
    );
  }

  // ── 5. Counterpart → the joined device (the native receive path) ────────
  const drainsBefore = countMarker(counterpart.surface.logs(counterpart.peer), 'queue_drained');
  const toJoined = 'e2e-s17-hello-from-the-counterpart';
  await counterpart.surface.openConversation?.(counterpart.peer, account.username, BUDGET);
  await counterpart.surface.sendMessage?.(counterpart.peer, toJoined, BUDGET);

  const sent = await waitForSignal(
    ctx,
    counterpart.surface,
    counterpart.peer,
    { label: 'message sent (counterpart)', marker: 'message_sent' },
    BUDGET,
  );
  if (sent.fields['conversation'] !== conversation) {
    throw new Error(
      `the message was sent in a different conversation: ${ctx.mask(sent.fields['conversation'] ?? '(none)')}`,
    );
  }
  const msgId = sent.fields['msg_id'];
  if (!msgId) {
    throw new Error(`event=message_sent has no msg_id field: ${ctx.mask(sent.line)}`);
  }
  const delivered = await waitForSignal(
    ctx,
    counterpart.surface,
    counterpart.peer,
    { label: 'message delivered (counterpart)', marker: 'message_delivered' },
    BUDGET,
  );
  if (delivered.fields['msg_id'] !== msgId) {
    throw new Error(
      `the delivered marker names another message: ${ctx.mask(delivered.fields['msg_id'] ?? '(none)')} vs ${ctx.mask(msgId)}`,
    );
  }
  const received = await waitForSignal(
    ctx,
    joined.surface,
    joined.peer,
    { label: 'message received (joined device)', marker: 'message_received' },
    BUDGET,
  );
  if (received.fields['msg_id'] !== msgId) {
    throw new Error(
      `the joined device saw another message: ${ctx.mask(received.fields['msg_id'] ?? '(none)')} vs ${ctx.mask(msgId)}`,
    );
  }
  const bubbles = (await counterpart.surface.readMessages?.(counterpart.peer)) ?? [];
  if (!bubbles.some((text) => text.includes(toJoined))) {
    throw new Error(
      `the sender did not render its own bubble; bubbles: ${JSON.stringify(bubbles)}`,
    );
  }
  recordObservation(
    ctx,
    joined,
    'message received by the joined device',
    `msg_id=${ctx.mask(msgId)} arrived on ${joined.surface.name}/${joined.peer}, sent from the counterpart account`,
  );
  // The queue-drain marker fires at the end of every drain cycle, so the
  // assertion is about a NEW drain after this send, counted from a baseline.
  await waitMarkerCount(
    ctx,
    counterpart,
    'queue_drained',
    drainsBefore + 1,
    'queue drained (counterpart)',
    BUDGET,
  );

  // ── 6. The joined device → counterpart, when it can send at all ─────────
  const openConversation = joined.surface.openConversation?.bind(joined.surface);
  const sendMessage = joined.surface.sendMessage?.bind(joined.surface);
  const toCounterpart = 'e2e-s17-hello-from-the-joined-device';
  // Overwritten by the deep-link branch below, whose relaunch replaces this peer's log stream — do not "simplify" that re-read away: a pre-relaunch count is not comparable to a post-relaunch one.
  let joinedDrainsBefore = countMarker(joined.surface.logs(joined.peer), 'queue_drained');

  if (openConversation && sendMessage) {
    // A DOM surface types the message into the composer like a person would.
    await openConversation(joined.peer, counterpartIdentity.username, BUDGET);
    await sendMessage(joined.peer, toCounterpart, BUDGET);
  } else if (joined.surface.caps.deepLink) {
    // No typing channel: `po://send?…` runs the app's own send path from a
    // launch URL — the harness-only action (`config::e2e::enabled()` gates the
    // link). `openUrl` relaunches the app, the shell claims the queued action
    // once it is up, and `deep_link_send` reports the outcome, so a refusal
    // names its reason instead of timing out here.
    await joined.surface.openUrl(
      joined.peer,
      sendLink(counterpartIdentity.username, toCounterpart),
    );
    const action = await waitForSignal(
      ctx,
      joined.surface,
      joined.peer,
      { label: 'deep-link send (joined device)', marker: 'deep_link_send' },
      BUDGET,
    );
    if (action.fields['accepted'] !== 'true') {
      throw new Error(
        `the deep-link send was refused (reason=${ctx.mask(action.fields['reason'] ?? '?')}): ` +
          ctx.mask(action.line),
      );
    }
    if (action.fields['conversation'] !== conversation) {
      throw new Error(
        `the deep-link send resolved another conversation: ` +
          `${ctx.mask(action.fields['conversation'] ?? '(none)')} vs ${ctx.mask(conversation)}`,
      );
    }
    recordObservation(
      ctx,
      joined,
      'send driven by deep link',
      `po://send?… ran the send path with no typing channel; accepted=true, conversation matches`,
    );
    // Re-read: `openUrl` replaced this peer's log stream (a pre-relaunch count is not comparable), and the seam's accepted marker is emitted where the op is enqueued, so a drain counted from here on is a drain after this send — the same claim the DOM branch's baseline makes.
    joinedDrainsBefore = countMarker(joined.surface.logs(joined.peer), 'queue_drained');
  } else {
    skip(
      ctx,
      joined,
      'sendMessage',
      `${joined.surface.name} has neither a typing channel nor a deep-link channel, ` +
        'so the joined device can only receive; the return direction is unverified there',
    );
    ctx.note(
      `S17 partial on ${anchor.surface.name}+${joined.surface.name}: the paired device joined the account, ` +
        'sent the invite and received a message; it cannot send one',
    );
    return;
  }

  const joinedSent = await waitForSignal(
    ctx,
    joined.surface,
    joined.peer,
    { label: 'message sent (joined device)', marker: 'message_sent' },
    BUDGET,
  );
  const joinedMsgId = joinedSent.fields['msg_id'];
  if (!joinedMsgId) {
    throw new Error(`event=message_sent has no msg_id field: ${ctx.mask(joinedSent.line)}`);
  }
  if (joinedSent.fields['conversation'] !== conversation) {
    throw new Error(
      `the joined device sent in a different conversation: ${ctx.mask(joinedSent.fields['conversation'] ?? '(none)')}`,
    );
  }
  const joinedDelivered = await waitForSignal(
    ctx,
    joined.surface,
    joined.peer,
    { label: 'message delivered (joined device)', marker: 'message_delivered' },
    BUDGET,
  );
  if (joinedDelivered.fields['msg_id'] !== joinedMsgId) {
    throw new Error(
      `the delivered marker names another message: ${ctx.mask(joinedDelivered.fields['msg_id'] ?? '(none)')} vs ${ctx.mask(joinedMsgId)}`,
    );
  }
  const counterpartReceivedMessage = await waitForSignal(
    ctx,
    counterpart.surface,
    counterpart.peer,
    { label: 'message received (counterpart)', marker: 'message_received' },
    BUDGET,
  );
  if (counterpartReceivedMessage.fields['msg_id'] !== joinedMsgId) {
    throw new Error(
      `the counterpart saw another message: ${ctx.mask(counterpartReceivedMessage.fields['msg_id'] ?? '(none)')} vs ${ctx.mask(joinedMsgId)}`,
    );
  }
  const counterpartBubbles = (await counterpart.surface.readMessages?.(counterpart.peer)) ?? [];
  if (!counterpartBubbles.some((text) => text.includes(toCounterpart))) {
    throw new Error(
      `the counterpart did not render the joined device's message; bubbles: ${JSON.stringify(counterpartBubbles)}`,
    );
  }
  recordObservation(
    ctx,
    joined,
    'message bubble rendered (counterpart)',
    `msg_id=${ctx.mask(joinedMsgId)}, ${counterpartBubbles.length} bubble(s), the joined device's text is among them`,
  );
  await waitMarkerCount(
    ctx,
    joined,
    'queue_drained',
    joinedDrainsBefore + 1,
    'queue drained (joined device)',
    BUDGET,
  );

  ctx.note(
    `S17 OK on ${anchor.surface.name}/${anchor.peer}(anchor) + ${counterpart.surface.name}/${counterpart.peer}` +
      `(counterpart) ↔ ${joined.surface.name}/${joined.peer}: the paired device joined the account from the ` +
      'pairing alone, took both directions of the conversation and rendered its own bubble',
  );
}
