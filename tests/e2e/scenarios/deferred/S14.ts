// S14 — Invite dismissal: the decline half of the invite journey.
//
// S2 covers ACCEPTING a parked invite; nothing covers refusing one. The
// user-visible promise of "Dismiss" is that the invite goes away and NO
// conversation is created — an invite the app accepted anyway would be silent
// and nasty: an unknown sender would end up with a live thread in the
// receiver's list.
//
// Observables: the Dismiss control on the parked-invite row (the parked row
// ITSELF — the only thing that makes the invite dismissible), the receiver's
// contact list still being empty, and the ABSENCE of `conversation_active` on
// either peer. The absence is counted against a baseline taken before the
// click, so a marker from an earlier phase of the run cannot satisfy it.
//
// NOT `event=invite_received`: that marker comes from `process_invite`
// (`src/chat/invite/processing.rs`), which `receiver.rs` reaches only once the
// sender is a KNOWN contact. This scenario deliberately sends from an unknown
// one, so the invite parks (`InviteParked`) and `invite_received` must never
// appear — waiting on it would fail exactly when the app behaves correctly.

import { sleep, waitFor } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { BUDGET, PASSWORD, contactLink, countMarker, pickPeers, skip } from './support.ts';

export const name = 'S14';
export const description =
  'Invite dismissal: a declined invite clears and creates no conversation on either side';

/** The Dismiss control on the parked-invite row — the button carries no id. */
const DISMISS = 'button:has-text("Dismiss")';
/**
 * How long a wrongly-accepted invite has to surface as `conversation_active`
 * before the absence assertion means anything. The accept path emits the
 * marker within a few seconds of its own network round trip (S2/S3), so this
 * is a settle window for a negative, not a wait for a positive.
 */
const SETTLE_MS = 5_000;

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
      'dom',
      alpha.surface.dom !== undefined,
      `${alpha.surface.name} cannot be driven through its DOM to press Dismiss`,
    )
  ) {
    return;
  }
  const dom = alpha.surface.dom?.bind(alpha.surface);
  if (!dom) return;

  await alpha.surface.signIn?.(alpha.peer, 'e2e-s14-alpha', PASSWORD, BUDGET);
  await beta.surface.signIn?.(beta.peer, 'e2e-s14-beta', PASSWORD, BUDGET);

  // B opens a conversation with A. A does not know B, so the invite is parked
  // instead of accepted — that park is what makes the Dismiss path reachable.
  const identity = await alpha.surface.identity?.(alpha.peer);
  if (!identity) {
    skip(ctx, alpha, 'identity', `${alpha.surface.name} reported no username/dht_key`);
    return;
  }
  await beta.surface.addContact?.(
    beta.peer,
    contactLink(identity.username, identity.dhtKey),
    BUDGET,
  );
  const dismissCount = async (): Promise<number> =>
    Number((await dom(alpha.peer, [{ readCount: ['rows', DISMISS] }]))['rows'] ?? 0);

  // The parked row is the observable that the invite arrived and parked: it is
  // what carries the Dismiss control this scenario is about, and unlike
  // `invite_received` it exists precisely because the sender is unknown.
  await dom(alpha.peer, [{ waitFor: [DISMISS, BUDGET] }]);
  const parkedRows = await dismissCount();

  // The baseline comes AFTER the park and BEFORE the click: the SENDER marks
  // its own side Active when its own conversation bootstrap finishes, which is
  // part of getting the invite here at all — counted from an earlier baseline
  // that activation reads as if the dismissal had caused it.
  const activeCounts = (): { alpha: number; beta: number } => ({
    alpha: countMarker(alpha.surface.logs(alpha.peer), 'conversation_active'),
    beta: countMarker(beta.surface.logs(beta.peer), 'conversation_active'),
  });
  const before = activeCounts();
  ctx.record({
    surface: alpha.surface.name,
    peer: alpha.peer,
    kind: 'step',
    label: 'invite parked',
    line: `Dismiss control rendered x${parkedRows}`,
  });
  if (parkedRows < 1) {
    throw new Error('S14: the parked invite row rendered no Dismiss control');
  }

  // ── Dismiss it ──────────────────────────────────────────────────────────
  await dom(alpha.peer, [{ click: DISMISS }]);
  await waitFor(
    async () => ((await dismissCount()) === 0 ? 'cleared' : undefined),
    { label: 'the pending invite row to clear after Dismiss', timeoutMs: BUDGET },
  );

  // The decide-the-negative half: holding the row open for another beat would
  // not make it more true, but an accept that is still in flight can only show
  // up later, so the settle window is what gives the assertion teeth.
  await sleep(SETTLE_MS);
  const after = activeCounts();
  if (after.alpha !== before.alpha) {
    throw new Error(
      `S14 FINDING: dismissing the invite reached conversation_active on the receiver ` +
        `(before=${before.alpha}, after=${after.alpha}) — Dismiss accepted the conversation`,
    );
  }
  if (after.beta !== before.beta) {
    throw new Error(
      `S14 FINDING: dismissing the invite reached conversation_active on the SENDER ` +
        `(before=${before.beta}, after=${after.beta}) — the dismissed invite still became a conversation`,
    );
  }

  // A refused invite must not have stored a contact either: a dismissal that
  // leaves a half-made contact would show up in the list as a dead row.
  const page = await dom(alpha.peer, [{ readText: ['text', 'body'] }]);
  const text = String(page['text'] ?? '');
  if (!text.includes('No contacts yet')) {
    throw new Error(
      `S14 FINDING: after dismissing an invite from an unknown sender the receiver's chat list ` +
        `is not empty — expected "No contacts yet" in ${JSON.stringify(text.slice(0, 200))}`,
    );
  }

  ctx.record({
    surface: alpha.surface.name,
    peer: alpha.peer,
    kind: 'log',
    label: 'invite dismissed',
    line: 'row cleared on the receiver, no contact stored, no conversation_active on either peer',
  });
  const shot = await alpha.surface.screenshot(alpha.peer, 'invite-dismissed');
  if (shot !== null) ctx.note(`screenshot: ${ctx.rel(shot)}`);
  ctx.note(`S14 OK on ${alpha.surface.name}: a dismissed invite created no conversation`);
}
