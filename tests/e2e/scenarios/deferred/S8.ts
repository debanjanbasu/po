// S8 — kill while a thread is open.
//
// Observables: `event=app_started` counted from a baseline taken at the kill
// (the relaunch really happened), and the recovered messages visible in the
// conversation again.
//
// B has the thread open when it is killed. On relaunch its conversation watch
// is re-armed, it re-reads the account's records, and the history is back.
//
// `event=watch_reconciled` is NOT asserted here, and cannot be while the seam
// reports only changes: both emitters are change-gated (`watcher/merge.rs`,
// `storage/store_trait`) and this account's state does not move while B is
// down, so the relaunch emits nothing — every captured run logged exactly one
// line, at startup, before the kill. The timeline records the count; making
// the reconcile itself observable needs a seam that reports the pull (not only
// the change).
import { waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import {
  BUDGET,
  PASSWORD,
  contactLink,
  countMarker,
  countMarkerWith,
  pickPeers,
  skip,
  recordIdentity,
  recordObservation,
  waitMarkerCount,
} from './support.ts';

export const name = 'S8';
export const description =
  'Kill with a thread open: the killed peer boots again and recovers the conversation history';

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
  if (!beta.surface.readMessages) missing.push(`${beta.surface.name}: readMessages`);
  if (!beta.surface.restart) missing.push(`${beta.surface.name}: restart`);
  if (missing.length > 0) {
    skip(ctx, alpha, 'kill-with-thread-open capabilities', missing.join('; '));
    return;
  }

  // Two independent accounts with an active conversation. Both peers only
  // SIGN IN, so neither runs the app's DHT gate (`assertDhtReady`'s doc).
  await alpha.surface.signIn?.(alpha.peer, 'e2e-s8-alpha', PASSWORD, BUDGET);
  await beta.surface.signIn?.(beta.peer, 'e2e-s8-beta', PASSWORD, BUDGET);
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

  // A sends a message, so there is history to recover.
  const text = 'e2e-s8-history';
  await beta.surface.openConversation?.(beta.peer, alphaIdentity.username, BUDGET);
  await alpha.surface.openConversation?.(alpha.peer, 'e2e-s8-beta', BUDGET);
  await alpha.surface.sendMessage?.(alpha.peer, text, BUDGET);
  const sent = await waitForSignal(
    ctx,
    alpha.surface,
    alpha.peer,
    { label: 'message sent (alpha)', marker: 'message_sent' },
    BUDGET,
  );
  const msgId = sent.fields['msg_id'];
  if (!msgId) {
    throw new Error(`event=message_sent has no msg_id field: ${ctx.mask(sent.line)}`);
  }
  await waitForSignal(
    ctx,
    beta.surface,
    beta.peer,
    { label: 'message received (beta)', marker: 'message_received' },
    BUDGET,
  );

  // Kill B with the thread open. Both markers a relaunch could be read from
  // fire EARLIER in this run, so each is counted from a baseline taken here:
  //   `app_started` — every boot, before any network work (`app/startup.rs`).
  //     This is the one a restart always produces.
  //   `watch_reconciled` — only when a pull actually CHANGED local state
  //     (`watcher/merge.rs`, `storage/store_trait`), so a restart with nothing
  //     new to merge emits none. It is recorded, not asserted: see the note
  //     after the restart.
  const startsBefore = countMarker(beta.surface.logs(beta.peer), 'app_started');
  const reconcilesBefore = countMarker(beta.surface.logs(beta.peer), 'watch_reconciled');
  await beta.surface.restart?.(beta.peer, {});
  ctx.record({
    surface: beta.surface.name,
    peer: beta.peer,
    kind: 'log',
    label: 'peer killed with the thread open',
    line: `conversation=${ctx.mask(conversation)}`,
  });

  // The relaunch itself: a NEW boot on the same peer, counted from the
  // baseline. Without this the post-kill steps could be satisfied by the
  // pre-kill process (the 2026-11-11 run's `watch_reconciled` match was a line
  // emitted 78 s before the kill).
  await waitMarkerCount(ctx, beta, 'app_started', startsBefore + 1, 'peer restarted (app_started)', BUDGET);

  // The relaunched peer re-arms its watch and re-reads the account's records.
  // A reconcile is reported only when that pull changed the local state, and
  // this account's state did not move while B was down — every captured S8 run
  // (2026-11-03 … 2026-11-11) logged exactly one `watch_reconciled`, at
  // startup, before the kill. So the marker is recorded for the timeline
  // rather than asserted; the claim this scenario can prove is the one below
  // (the history is back in a process that booted after the kill).
  recordObservation(
    ctx,
    beta,
    'watch reconcile after the restart',
    `event=watch_reconciled occurrences=${reconcilesBefore} (all before the kill); a restart whose pull changes nothing emits none`,
  );

  // The history is back: the message is rendered again in the thread.
  await beta.surface.openConversation?.(beta.peer, alphaIdentity.username, BUDGET);
  const bubbles = (await beta.surface.readMessages?.(beta.peer)) ?? [];
  if (!bubbles.some((line) => line.includes(text))) {
    throw new Error(
      `the recovered conversation does not show the message; bubbles: ${JSON.stringify(bubbles)}`,
    );
  }
  const receivedCount = countMarkerWith(
    beta.surface.logs(beta.peer),
    'message_received',
    'msg_id',
    msgId,
  );
  recordObservation(
    ctx,
    beta,
    'history recovered',
    `msg_id=${ctx.mask(msgId)}, ${bubbles.length} bubble(s), message_received occurrences=${receivedCount}`,
  );

  ctx.note(
    `S8 OK on ${beta.surface.name}/${beta.peer}(killed)+${alpha.surface.name}/${alpha.peer}: ` +
      'the killed peer booted again and the conversation history came back',
  );
}
