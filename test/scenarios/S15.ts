// S15 — Typing indicator: a real peer signal, not a send.
//
// The only user-visible half of the typing exchange is the receiver's bubble,
// and nothing in the suite covered it: S3 covers message/delivery/read-side
// delivery and stops before typing. Two promises are worth an assertion:
//
//   1. typing in the composer reaches the OTHER peer's open conversation
//      (SignalPayload::Typing → the receiver's is_typing state), and
//   2. it is a *signal*, not a message — it must clear on its own
//      (`config::typing::EXPIRY_MS`, 5 s) and must never become a bubble the
//      sender or receiver sees in the transcript.
//
// The indicator is rendered inside the message column and only in its
// non-empty branch (`conversation_view.rs`: `TypingIndicator` sits in the
// `else` of `message_list.is_empty()`), so the flow sends one seed message
// first — a conversation with no messages can never show the bubble, and
// asserting 0 there would prove nothing.
//
// The bubble has no id and no aria label; `#chat-messages .loading-dots` is
// the whole DOM signature (`span.loading.loading-dots` is used by nothing
// else in this view — the send spinner is `loading-spinner`).

import { sleep, waitFor, waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { BUDGET, PASSWORD, contactLink, countMarker, pickPeers, skip } from './support.ts';

export const name = 'S15';
export const description =
  'Typing indicator: the composer reaches the peer as a typing signal, expires on its own, and never becomes a message';

/** The remote-typing bubble: `div.chat-bubble.opacity-50 > span.loading-dots`. */
const TYPING_BUBBLE = '#chat-messages .loading-dots';
const COMPOSER = 'textarea[aria-label="Message"]';
/** `config::typing::EXPIRY_MS` is 5000 ms; the settle leaves room for the render. */
const EXPIRY_SETTLE_MS = 7_000;
/** How long the receiver has to see a bubble after the sender's keystroke. */
const TYPING_TIMEOUT_MS = 30_000;

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
      'identity/addContact/acceptInvite',
      alpha.surface.identity !== undefined &&
        beta.surface.addContact !== undefined &&
        alpha.surface.acceptInvite !== undefined,
      'a surface cannot build an active conversation between two accounts',
    )
  ) {
    return;
  }
  if (
    !surfacesNeed(
      'openConversation/sendMessage/readMessages',
      alpha.surface.openConversation !== undefined &&
        alpha.surface.sendMessage !== undefined &&
        beta.surface.openConversation !== undefined &&
        beta.surface.readMessages !== undefined,
      'a surface cannot open a conversation, send a message or read the rendered bubbles',
    )
  ) {
    return;
  }
  if (
    !surfacesNeed(
      'dom',
      alpha.surface.dom !== undefined && beta.surface.dom !== undefined,
      `both peers must be DOM-driven (the sender types into its composer without sending, ` +
        `the receiver is read back): ${alpha.surface.name} dom=${alpha.surface.dom !== undefined}, ` +
        `${beta.surface.name} dom=${beta.surface.dom !== undefined}`,
    )
  ) {
    return;
  }
  const alphaDom = alpha.surface.dom?.bind(alpha.surface);
  const betaDom = beta.surface.dom?.bind(beta.surface);
  if (!alphaDom || !betaDom) return;

  // ── Two accounts, one active conversation (the S2 bootstrap) ────────────
  await alpha.surface.signIn?.(alpha.peer, 'e2e-s15-alpha', PASSWORD, BUDGET);
  await beta.surface.signIn?.(beta.peer, 'e2e-s15-beta', PASSWORD, BUDGET);
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
  await alpha.surface.acceptInvite?.(alpha.peer, BUDGET);
  await waitForSignal(
    ctx,
    alpha.surface,
    alpha.peer,
    { label: 'conversation active', marker: 'conversation_active' },
    BUDGET,
  );

  await beta.surface.openConversation?.(beta.peer, 'e2e-s15-alpha', BUDGET);
  await alpha.surface.openConversation?.(alpha.peer, 'e2e-s15-beta', BUDGET);

  // ── Seed message: the typing bubble only exists above a non-empty list ──
  const seed = 'e2e-s15-seed';
  const receivedBefore = countMarker(beta.surface.logs(beta.peer), 'message_received');
  await alpha.surface.sendMessage?.(alpha.peer, seed, BUDGET);
  await waitFor(
    () =>
      countMarker(beta.surface.logs(beta.peer), 'message_received') > receivedBefore
        ? 'received'
        : undefined,
    { label: 'the seed message to reach the receiver', timeoutMs: BUDGET },
  );

  const bubbleCount = async (peer: string, dom: typeof alphaDom): Promise<number> =>
    Number((await dom(peer, [{ readCount: ['dots', TYPING_BUBBLE] }]))['dots'] ?? 0);

  if ((await bubbleCount(beta.peer, betaDom)) !== 0) {
    throw new Error('S15: a typing bubble was already rendered before anyone typed');
  }

  // ── B types without sending ─────────────────────────────────────────────
  // `fill` dispatches one `input` event, which is exactly the app's typing
  // trigger (`MessageComposer`'s oninput → send_typing, throttled to one
  // signal per 3 s with the first always sent).
  await betaDom(beta.peer, [
    { waitFor: [COMPOSER, BUDGET] },
    { fill: [COMPOSER, 'e2e-s15-typing-not-sent'] },
  ]);

  await alphaDom(alpha.peer, [{ waitFor: [TYPING_BUBBLE, TYPING_TIMEOUT_MS] }]);
  const shown = await bubbleCount(alpha.peer, alphaDom);
  const ownEcho = await bubbleCount(beta.peer, betaDom);
  ctx.record({
    surface: alpha.surface.name,
    peer: alpha.peer,
    kind: 'step',
    label: 'typing visible to the peer',
    line: `receiver bubble x${shown}, sender's own view x${ownEcho}`,
  });
  if (shown !== 1) {
    throw new Error(`S15 FINDING: the peer's typing bubble rendered x${shown}, expected exactly 1`);
  }
  if (ownEcho !== 0) {
    throw new Error(
      `S15 FINDING: the sender rendered its own typing bubble (x${ownEcho}) — typing must be remote-only`,
    );
  }

  // ── It is a signal: it must expire on its own, and never become a message ─
  await sleep(EXPIRY_SETTLE_MS);
  const expired = await bubbleCount(alpha.peer, alphaDom);
  if (expired !== 0) {
    throw new Error(
      `S15 FINDING: the typing bubble was still rendered ${EXPIRY_SETTLE_MS}ms after the sender's ` +
        `keystroke (x${expired}) — the app's typing expiry did not clear it`,
    );
  }
  const bubbles = (await beta.surface.readMessages?.(beta.peer)) ?? [];
  if (bubbles.length !== 1) {
    throw new Error(
      `S15 FINDING: typing reached the transcript — the sender's list holds ${bubbles.length} ` +
        `bubble(s), expected 1 (${JSON.stringify(bubbles)})`,
    );
  }

  ctx.record({
    surface: alpha.surface.name,
    peer: alpha.peer,
    kind: 'log',
    label: 'typing signal lifecycle',
    line: 'bubble appeared for the peer, was not echoed to the sender, expired on its own, and left no message',
  });
  ctx.note(`S15 OK on ${alpha.surface.name}: the peer saw the typing bubble and it expired`);
}
