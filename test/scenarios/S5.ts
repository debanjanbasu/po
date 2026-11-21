// S5 — mid-send kill.
//
// Observables (docs/device-testing.md § S5): the queue drains once
// (`event=queue_drained`), and the receiver sees the message EXACTLY once
// (`event=message_received` once) — the pending-op dedupe must not replay it
// and no tombstone may fire.
//
// B is force-quit right after the send is queued; on relaunch its pending op
// drains and the message lands on A exactly once.
import { waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import {
  BUDGET,
  PASSWORD,
  contactLink,
  countMarker,
  countMarkerWith,
  lastMarkerLine,
  pickPeers,
  recordIdentity,
  recordObservation,
  skip,
  waitMarkerCount,
} from './support.ts';

export const name = 'S5';
export const description =
  'Mid-send kill: the relaunched peer drains the queue once and the receiver sees the message exactly once';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  const { first: alpha, second: beta } = pickPeers(surfaces);

  const missing: string[] = [];
  if (!alpha.surface.signIn || !beta.surface.signIn) {
    missing.push('signIn on both peers');
  }
  if (!alpha.surface.identity) missing.push(`${alpha.surface.name}: identity`);
  if (!beta.surface.addContact) missing.push(`${beta.surface.name}: addContact`);
  if (!alpha.surface.acceptInvite) missing.push(`${alpha.surface.name}: acceptInvite`);
  if (!beta.surface.openConversation || !beta.surface.sendMessage) {
    missing.push(`${beta.surface.name}: openConversation/sendMessage`);
  }
  if (!beta.surface.restart) missing.push(`${beta.surface.name}: restart`);
  if (missing.length > 0) {
    skip(ctx, alpha, 'mid-send-kill capabilities', missing.join('; '));
    return;
  }

  // Two independent accounts with an active conversation. Both peers only
  // SIGN IN, so neither runs the app's DHT gate (`assertDhtReady`'s doc).
  await alpha.surface.signIn?.(alpha.peer, 'e2e-s5-alpha', PASSWORD, BUDGET);
  await beta.surface.signIn?.(beta.peer, 'e2e-s5-beta', PASSWORD, BUDGET);
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
  await waitForSignal(
    ctx,
    alpha.surface,
    alpha.peer,
    { label: 'conversation active', marker: 'conversation_active' },
    BUDGET,
  );

  // B sends, and is killed right after the message was queued for delivery.
  const text = 'e2e-s5-mid-send';
  await beta.surface.openConversation?.(beta.peer, alphaIdentity.username, BUDGET);
  const receivedBefore = countMarkerWith(
    alpha.surface.logs(alpha.peer),
    'message_received',
    'from',
    'e2e-s5-beta',
  );
  await beta.surface.sendMessage?.(beta.peer, text, BUDGET);
  const sent = await waitForSignal(
    ctx,
    beta.surface,
    beta.peer,
    { label: 'message sent (beta)', marker: 'message_sent' },
    BUDGET,
  );
  const msgId = sent.fields['msg_id'];
  if (!msgId) {
    throw new Error(`event=message_sent has no msg_id field: ${ctx.mask(sent.line)}`);
  }

  // Kill B before the delivery lands. The drain marker fires at the end of
  // every drain cycle, so the assertion counts drains from a baseline taken
  // before the kill: the relaunch must drain the queue ONCE more.
  const drainsBefore = countMarker(beta.surface.logs(beta.peer), 'queue_drained');
  await beta.surface.restart?.(beta.peer, {});
  ctx.record({
    surface: beta.surface.name,
    peer: beta.peer,
    kind: 'log',
    label: 'peer killed mid-send',
    line: `msg_id=${ctx.mask(msgId)}`,
  });

  // The relaunched peer drains the queue once.
  const drained = await waitMarkerCount(
    ctx,
    beta,
    'queue_drained',
    drainsBefore + 1,
    'queue drained after the kill',
    BUDGET,
  );
  if (drained.fields['dead'] !== '0') {
    throw new Error(
      `the queue dead-lettered work after the kill: ${ctx.mask(drained.fields['dead'] ?? '(none)')}`,
    );
  }

  // The receiver sees the message exactly once.
  await waitMarkerCount(
    ctx,
    alpha,
    'message_received',
    receivedBefore + 1,
    'message received (alpha)',
    BUDGET,
  );
  const receivedTotal = countMarkerWith(
    alpha.surface.logs(alpha.peer),
    'message_received',
    'msg_id',
    msgId,
  );
  if (receivedTotal !== 1) {
    throw new Error(
      `the receiver saw the message ${receivedTotal} times (expected exactly 1): ` +
        'the pending op was replayed or a tombstone fired',
    );
  }
  ctx.record({
    surface: alpha.surface.name,
    peer: alpha.peer,
    kind: 'log',
    label: 'message received exactly once',
    line: `msg_id=${ctx.mask(msgId)} occurrences=${receivedTotal}`,
  });

  // A tombstone replay would show up as a dead-letter or a second delivery.
  const deadLettered = countMarker(beta.surface.logs(beta.peer), 'dead_lettered');
  if (deadLettered > 0) {
    throw new Error(
      `the relaunched peer dead-lettered ${deadLettered} op(s) — the kill lost the pending op`,
    );
  }
  // Negative assertions are invisible unless recorded as their own kind.
  const deadLine = lastMarkerLine(beta.surface.logs(beta.peer), 'dead_lettered');
  recordObservation(
    ctx,
    beta,
    'no dead letter after the kill',
    deadLine === undefined
      ? `event=dead_lettered absent across ${deadLettered} op(s)`
      : `event=dead_lettered seen before the kill: ${deadLine}`,
  );

  ctx.note(
    `S5 OK on ${beta.surface.name}/${beta.peer}(killed)+${alpha.surface.name}/${alpha.peer}(receiver): ` +
      'the queue drained once and the receiver saw the message exactly once',
  );
}
