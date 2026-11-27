// S3 — messaging both directions.
//
// Observables (docs/device-testing.md § S3): `event=message_sent` and
// `event=message_delivered` on the sender, `event=message_received` on the
// receiver, `event=queue_drained` on the sender once the queue is empty, and
// the receiver renders the bubble.
//
// The two peers are two independent accounts that already share an active
// conversation (the conversation is bootstrapped by S2; a scenario run on its
// own needs the contact to exist first, so this scenario bootstraps it the
// same way S2 does).
import { waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import {
  BUDGET,
  PASSWORD,
  contactLink,
  countMarker,
  pickPeers,
  recordIdentity,
  recordObservation,
  skip,
  waitMarkerCount,
} from './support.ts';

export const name = 'S3';
export const description =
  'Messaging both directions: message_sent, message_delivered, message_received, queue_drained';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  const { first: alpha, second: beta } = pickPeers(surfaces);
  const surfacesNeed = (cap: string, ok: boolean, why: string): boolean => {
    if (ok) return true;
    skip(ctx, alpha, cap, why);
    return false;
  };

  if (
    !surfacesNeed(
      'signIn',
      alpha.surface.signIn !== undefined && beta.surface.signIn !== undefined,
      `both peers need their own account: ${alpha.surface.name} signIn=${alpha.surface.signIn !== undefined}, ` +
        `${beta.surface.name} signIn=${beta.surface.signIn !== undefined}`,
    )
  ) {
    return;
  }
  if (
    !surfacesNeed(
      'identity',
      alpha.surface.identity !== undefined,
      `${alpha.surface.name} cannot report the peer's own identity`,
    )
  ) {
    return;
  }
  if (
    !surfacesNeed(
      'addContact',
      beta.surface.addContact !== undefined,
      `${beta.surface.name} cannot deliver a contact link`,
    )
  ) {
    return;
  }
  if (
    !surfacesNeed(
      'acceptInvite',
      alpha.surface.acceptInvite !== undefined,
      `${alpha.surface.name} cannot accept the pending invite`,
    )
  ) {
    return;
  }
  if (
    !surfacesNeed(
      'openConversation/sendMessage/readMessages',
      alpha.surface.openConversation !== undefined &&
        alpha.surface.sendMessage !== undefined &&
        alpha.surface.readMessages !== undefined &&
        beta.surface.openConversation !== undefined &&
        beta.surface.sendMessage !== undefined &&
        beta.surface.readMessages !== undefined,
      'a surface cannot open a conversation, send a message or read the rendered bubbles',
    )
  ) {
    return;
  }

  // Two independent accounts and an active conversation between them. Both
  // peers only SIGN IN, so neither runs the app's DHT gate
  // (`assertDhtReady`'s doc): no gate assertion here.
  await alpha.surface.signIn?.(alpha.peer, 'e2e-s3-alpha', PASSWORD, BUDGET);
  await beta.surface.signIn?.(beta.peer, 'e2e-s3-beta', PASSWORD, BUDGET);
  const alphaIdentity = await recordIdentity(ctx, alpha);
  if (!alphaIdentity) {
    skip(ctx, alpha, 'identity', `${alpha.surface.name} could not report the peer's identity`);
    return;
  }
  await beta.surface.addContact?.(
    beta.peer,
    contactLink(alphaIdentity.username, alphaIdentity.dhtKey),
    BUDGET,
  );
  await alpha.surface.acceptInvite?.(alpha.peer, BUDGET);
  const active = await waitForSignal(
    ctx,
    alpha.surface,
    alpha.peer,
    { label: 'conversation active', marker: 'conversation_active' },
    BUDGET,
  );
  const conversation = active.fields['conversation'];
  if (!conversation) {
    throw new Error(`event=conversation_active has no conversation field: ${ctx.mask(active.line)}`);
  }

  // A → B.
  // The queue-drain marker fires at the end of every drain cycle, so the
  // assertion is about a NEW drain after this send, counted from a baseline.
  const drainsBefore = countMarker(alpha.surface.logs(alpha.peer), 'queue_drained');
  const toBeta = 'e2e-s3-hello-from-alpha';
  await beta.surface.openConversation?.(beta.peer, alphaIdentity.username, BUDGET);
  await alpha.surface.openConversation?.(alpha.peer, 'e2e-s3-beta', BUDGET);
  await alpha.surface.sendMessage?.(alpha.peer, toBeta, BUDGET);

  const sent = await waitForSignal(
    ctx,
    alpha.surface,
    alpha.peer,
    { label: 'message sent (alpha)', marker: 'message_sent' },
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
    alpha.surface,
    alpha.peer,
    { label: 'message delivered (alpha)', marker: 'message_delivered' },
    BUDGET,
  );
  if (delivered.fields['msg_id'] !== msgId) {
    throw new Error(
      `the delivered marker names another message: ${ctx.mask(delivered.fields['msg_id'] ?? '(none)')} vs ${ctx.mask(msgId)}`,
    );
  }

  const received = await waitForSignal(
    ctx,
    beta.surface,
    beta.peer,
    { label: 'message received (beta)', marker: 'message_received' },
    BUDGET,
  );
  if (received.fields['msg_id'] !== msgId) {
    throw new Error(
      `the receiver saw another message: ${ctx.mask(received.fields['msg_id'] ?? '(none)')} vs ${ctx.mask(msgId)}`,
    );
  }

  // The receiver renders the bubble.
  const bubbles = (await beta.surface.readMessages?.(beta.peer)) ?? [];
  if (!bubbles.some((text) => text.includes(toBeta))) {
    throw new Error(
      `the receiver did not render the message bubble; bubbles: ${JSON.stringify(bubbles)}`,
    );
  }
  recordObservation(
    ctx,
    beta,
    'message bubble rendered (beta)',
    `msg_id=${ctx.mask(msgId)}, ${bubbles.length} bubble(s), the sent text is among them`,
  );

  // B → A.
  // Baseline the counts first: direction 1 already emitted message_sent /
  // message_received, and a bare waitForSignal would match those stale lines
  // even if direction 2 silently no-opped (same pattern as S5's restart leg).
  const betaSentBefore = countMarker(beta.surface.logs(beta.peer), 'message_sent');
  const alphaReceivedBefore = countMarker(alpha.surface.logs(alpha.peer), 'message_received');
  const toAlpha = 'e2e-s3-hello-from-beta';
  await alpha.surface.openConversation?.(alpha.peer, 'e2e-s3-beta', BUDGET);
  await beta.surface.openConversation?.(beta.peer, alphaIdentity.username, BUDGET);
  await beta.surface.sendMessage?.(beta.peer, toAlpha, BUDGET);

  const betaSent = await waitMarkerCount(
    ctx,
    beta,
    'message_sent',
    betaSentBefore + 1,
    'message sent (beta)',
    BUDGET,
  );
  const betaMsgId = betaSent.fields['msg_id'];
  if (!betaMsgId) {
    throw new Error(`event=message_sent has no msg_id field: ${ctx.mask(betaSent.line)}`);
  }
  const betaDelivered = await waitForSignal(
    ctx,
    beta.surface,
    beta.peer,
    { label: 'message delivered (beta)', marker: 'message_delivered' },
    BUDGET,
  );
  if (betaDelivered.fields['msg_id'] !== betaMsgId) {
    throw new Error(
      `the delivered marker names another message: ${ctx.mask(betaDelivered.fields['msg_id'] ?? '(none)')}`,
    );
  }
  const alphaReceived = await waitMarkerCount(
    ctx,
    alpha,
    'message_received',
    alphaReceivedBefore + 1,
    'message received (alpha)',
    BUDGET,
  );
  if (alphaReceived.fields['msg_id'] !== betaMsgId) {
    throw new Error(
      `the receiver saw another message: ${ctx.mask(alphaReceived.fields['msg_id'] ?? '(none)')}`,
    );
  }
  const alphaBubbles = (await alpha.surface.readMessages?.(alpha.peer)) ?? [];
  if (!alphaBubbles.some((text) => text.includes(toAlpha))) {
    throw new Error(
      `the receiver did not render the message bubble; bubbles: ${JSON.stringify(alphaBubbles)}`,
    );
  }
  recordObservation(
    ctx,
    alpha,
    'message bubble rendered (alpha)',
    `msg_id=${ctx.mask(betaMsgId)}, ${alphaBubbles.length} bubble(s), the sent text is among them`,
  );

  // The sender's queue drains once the message left the device.
  await waitMarkerCount(
    ctx,
    alpha,
    'queue_drained',
    drainsBefore + 1,
    'queue drained (alpha)',
    BUDGET,
  );

  ctx.note(
    `S3 OK on ${alpha.surface.name}/${alpha.peer} ↔ ${beta.surface.name}/${beta.peer}: ` +
      'both directions sent, delivered and rendered',
  );
}
