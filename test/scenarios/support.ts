// oxlint-disable max-lines -- one file for the shared scenario building blocks: peer selection, marker helpers and the pairing leg live together because every scenario reads them as one vocabulary
// Shared building blocks for the tandem scenarios: peer selection, capability
// gating (a missing capability is a recorded skip, never a throw), the contact
// deep-link format the app really uses, and marker helpers that always go
// through the marker vocabulary (`event=<name>`), never a hand-rolled regex.

import { Buffer } from 'node:buffer';
import { parseFields, stripAnsi, waitFor, waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { SignalHit, Surface } from '../lib/types.ts';

/** The account password every scenario uses (same constant as S1). */
export const PASSWORD = 'E2e-Passw0rd!x';

/** Scenario budget. The app waits on Veilid's real DHT gate before its first
 *  record open (measured 3s–16s after launch), so nothing runs on less. */
export const BUDGET = 120_000;

export interface PeerRef {
  surface: Surface;
  peer: string;
}

/** The first two peers of the run: the anchor and its counterpart. */
export function pickPeers(surfaces: Surface[]): { first: PeerRef; second: PeerRef } {
  if (surfaces.length === 0) throw new Error('scenario needs at least one surface');
  const first = surfaces[0];
  const second = surfaces[1];
  if (surfaces.length > 1) {
    return {
      first: { surface: first, peer: first.peerIds[0] },
      second: { surface: second, peer: second.peerIds[0] },
    };
  }
  const peer = first.peerIds[1];
  if (!peer) {
    throw new Error(
      `scenario needs two peers; ${first.name} hosts only ${first.peerIds.join(',')}`,
    );
  }
  return {
    first: { surface: first, peer: first.peerIds[0] },
    second: { surface: first, peer },
  };
}

/** Record a skip: the surface cannot run this step, and why. Never throws. */
export function skip(ctx: RunContext, ref: PeerRef, capability: string, reason: string): void {
  ctx.record({
    surface: ref.surface.name,
    peer: ref.peer,
    kind: 'skip',
    label: `skipped: ${capability}`,
    line: reason,
  });
  ctx.note(`SKIP ${ref.surface.name}/${ref.peer}: missing "${capability}" — ${reason}`);
}

/** The contact deep link the app itself builds (`share_urls::contact_deep_link`). */
export function contactLink(username: string, dhtKey: string): string {
  return `po://chat?data=${Buffer.from(`${username}|${dhtKey}`, 'utf8').toString('base64url')}`;
}

/**
 * The harness-only send deep link (`config::deep_link::SEND_SCHEME`), which
 * runs the app's ordinary send path for `contact`'s conversation — the send
 * channel a surface with no typing channel has. Parsed only while the app's
 * `e2e` seam is on (`config::e2e::enabled()`).
 *
 * Values go in verbatim: the app reads them without percent-decoding, so a
 * text containing `&` or `#` would truncate.
 */
export function sendLink(contact: string, text: string): string {
  return `po://send?to=${contact}&text=${text}`;
}

/** Put an observation on the run's timeline. What a scenario saw must be as
 *  visible as what it waited for: a PASS whose timeline only shows the waits
 *  hides the assertions (and every negative one entirely). */
export function recordObservation(
  ctx: RunContext,
  ref: PeerRef,
  label: string,
  line: string,
): void {
  ctx.record({ surface: ref.surface.name, peer: ref.peer, kind: 'log', label, line: ctx.mask(line) });
}

/** Every log line carrying `event=<marker>`. */
function markerLines(text: string, marker: string): string[] {
  const re = new RegExp(`event\\s*=\\s*"?${marker}"?(?=\\s|;|$)`, 'u');
  return stripAnsi(text)
    .split('\n')
    .filter((line) => re.test(line));
}

export function countMarker(text: string, marker: string): number {
  return markerLines(text, marker).length;
}

/** The last `event=<marker>` line, or undefined. A counting helper is pure —
 *  it records nothing — so it stays usable inside `waitFor` probes. */
export function lastMarkerLine(text: string, marker: string): string | undefined {
  const lines = markerLines(text, marker);
  return lines.length > 0 ? lines[lines.length - 1] : undefined;
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&');
}

/** How many `event=<marker>` lines also carry `field=value`. */
export function countMarkerWith(
  text: string,
  marker: string,
  field: string,
  value: string,
): number {
  const re = new RegExp(`\\b${escapeRegExp(field)}=${escapeRegExp(value)}(?=\\s|$)`, 'u');
  return markerLines(text, marker).filter((line) => re.test(line)).length;
}

/** Wait for a marker whose field matches an exact value (e.g. `pending=0`). */
export async function waitMarkerField(
  ctx: RunContext,
  ref: PeerRef,
  marker: string,
  field: string,
  value: string,
  label: string,
  timeoutMs: number,
): Promise<SignalHit> {
  const started = Date.now();
  const hit = await waitFor(
    () => {
      for (const line of markerLines(ref.surface.logs(ref.peer), marker)) {
        const fields = parseFields(line);
        if (fields[field] === value) {
          return { kind: 'marker' as const, label, line, fields };
        }
      }
      return undefined;
    },
    { label, timeoutMs },
  );
  const result: SignalHit = { ...hit, ms: Date.now() - started };
  ctx.record({
    surface: ref.surface.name,
    peer: ref.peer,
    kind: result.kind,
    label: result.label,
    line: ctx.mask(result.line),
  });
  return result;
}

/**
 * Wait until `marker` has fired at least `atLeast` times.
 *
 * `findMarker` returns the LAST matching line, so a marker that already fired
 * earlier in the run (a queue drain, a delivered message) would satisfy a
 * plain wait instantly. Counting from a baseline taken before the action
 * keeps the assertion about THIS action.
 */
export async function waitMarkerCount(
  ctx: RunContext,
  ref: PeerRef,
  marker: string,
  atLeast: number,
  label: string,
  timeoutMs: number,
): Promise<SignalHit> {
  const started = Date.now();
  const hit = await waitFor(
    () => {
      const lines = markerLines(ref.surface.logs(ref.peer), marker);
      if (lines.length < atLeast) return undefined;
      const line = lines[lines.length - 1];
      return { kind: 'marker' as const, label, line, fields: parseFields(line) };
    },
    { label, timeoutMs },
  );
  const result: SignalHit = { ...hit, ms: Date.now() - started };
  ctx.record({
    surface: ref.surface.name,
    peer: ref.peer,
    kind: result.kind,
    label: result.label,
    line: ctx.mask(result.line),
  });
  return result;
}

/**
 * The DHT gate opened on this peer and never timed out: `event=dht_ready`
 * must be in the log, and `event=dht_ready_timeout` must never appear.
 *
 * **The joining path only.** `dht_ready` has exactly one emitter —
 * `wait_for_dht_readiness` (`src/veilid/core/mod.rs`) — and its only caller is
 * the pairing/join flow (`src/user/pairing.rs`). A peer that merely signs in
 * never runs it, so asserting this marker on a signed-in peer can only time
 * out: the app is fine (`Veilid network attached successfully`,
 * `PublicInternet ready`) and the marker simply never comes. Call it for the
 * peer that joins — S1's joiner, S4's paired device, S17's joiner — and
 * nothing else.
 *
 * Where a peer just signs in, wait on the thing the scenario is about (its own
 * `event=…` marker, or a DOM control) instead; the DHT gate is then covered
 * transitively, because nothing the scenario asserts can happen before it.
 * Polled because the wait runs concurrently with the caller's own startup
 * steps; the timeout names the gate as the failure, not the symptom downstream.
 */
export async function assertDhtReady(ctx: RunContext, ref: PeerRef, timeoutMs: number): Promise<void> {
  try {
    const line = await waitFor(() => lastMarkerLine(ref.surface.logs(ref.peer), 'dht_ready'), {
      label: `event=dht_ready on ${ref.surface.name}/${ref.peer}`,
      timeoutMs,
    });
    recordObservation(ctx, ref, 'dht ready', line);
  } catch (err) {
    const timedOut = lastMarkerLine(ref.surface.logs(ref.peer), 'dht_ready_timeout');
    if (timedOut !== undefined) {
      throw new Error(
        `the DHT never came ready on ${ref.surface.name}/${ref.peer} — the app's own gate lapsed: ${ctx.mask(timedOut)}`,
        { cause: err },
      );
    }
    throw new Error(`the DHT never came ready on ${ref.surface.name}/${ref.peer}`, { cause: err });
  }
  const timedOutAfter = lastMarkerLine(ref.surface.logs(ref.peer), 'dht_ready_timeout');
  if (timedOutAfter !== undefined) {
    throw new Error(
      `the DHT-gate wait timed out on ${ref.surface.name}/${ref.peer} after reporting ready: ${ctx.mask(timedOutAfter)}`,
    );
  }
}

/**
 * A peer's own identity, recorded: the `event=identity` marker is what the
 * surface's `identity()` reader consumes underneath, and four scenarios build
 * their contact link from it, so the observation belongs on the timeline.
 * Null when the surface cannot report one (a skip is recorded, never thrown).
 */
export async function recordIdentity(
  ctx: RunContext,
  ref: PeerRef,
): Promise<{ username: string; dhtKey: string } | null> {
  const identity = ref.surface.identity?.bind(ref.surface);
  if (!identity) {
    skip(ctx, ref, 'identity', `${ref.surface.name} cannot report the peer's own identity`);
    return null;
  }
  const identityFields = await identity(ref.peer);
  const line = lastMarkerLine(ref.surface.logs(ref.peer), 'identity');
  recordObservation(
    ctx,
    ref,
    'identity reported',
    line ?? `username=${identityFields.username} dht_key=${identityFields.dhtKey}`,
  );
  return identityFields;
}

/**
 * Whether this peer can join an account at all: a DOM surface fills the
 * pairing form (`openPair`), a native one needs the launch environment and a
 * deep-link channel to route to that form.
 */
export function canJoin(joiner: PeerRef): boolean {
  return (
    joiner.surface.openPair !== undefined ||
    (joiner.surface.caps.env && joiner.surface.caps.deepLink && joiner.surface.relaunch !== undefined)
  );
}

/**
 * The joiner's half of pairing: get it into the anchor's account from whatever
 * channel it has.
 *
 * A surface with a DOM fills the pairing form itself (`openPair`). A native
 * joiner has no typing channel, so the payload and credentials ride its launch
 * environment (`PO_E2E_PAIR_*`) and the deep link is delivered separately: the
 * form reads the launch env as it mounts, so the relaunch comes first. The
 * `deep_link` marker is asserted because the env seams can complete a pairing
 * on their own — without it, a broken deep-link delivery would pass unnoticed.
 *
 * Throws: the callers that need a skippable answer gate on `canJoin` first.
 */
export async function joinPairedDevice(
  ctx: RunContext,
  joiner: PeerRef,
  share: { pin: string; deepLink: string },
  password: string,
  budget: number,
): Promise<void> {
  const openPair = joiner.surface.openPair?.bind(joiner.surface);
  if (openPair) {
    await openPair(joiner.peer, share.deepLink, share.pin, password, budget);
    return;
  }
  const relaunch = joiner.surface.relaunch?.bind(joiner.surface);
  if (!joiner.surface.caps.env || !relaunch) {
    throw new Error(`${joiner.surface.name}: surface cannot open the pairing form (no openPair helper)`);
  }
  await relaunch(joiner.peer, {
    PO_E2E_PAIR_PAYLOAD: share.deepLink,
    PO_E2E_PAIR_PIN: share.pin,
    PO_E2E_PAIR_PASSWORD: password,
    PO_E2E_ACCOUNT_PIN: '123456',
  });
  if (!joiner.surface.caps.deepLink) {
    throw new Error(
      `${joiner.surface.name}: those env seams only prefill the pairing form, so a native joiner also needs ` +
        'the deep link delivered before the app will route to it — this surface has no deep-link channel',
    );
  }
  await joiner.surface.openUrl(joiner.peer, share.deepLink);
  await waitForSignal(
    ctx,
    joiner.surface,
    joiner.peer,
    { label: 'deep link accepted by the app', marker: 'deep_link' },
    budget,
  );
}

/**
 * Sign the anchor in and pair the second peer into the SAME account.
 *
 * Two devices of one account is what the profile/dictionary/watch scenarios
 * need (the identity record is per account). Returns the reason it could not
 * happen instead of throwing, so the caller can record a skip.
 */
export async function pairSecondDevice(
  ctx: RunContext,
  anchor: PeerRef,
  joiner: PeerRef,
  username: string,
  budget: number,
): Promise<
  { ok: true; anchorIdentity: { username: string; dhtKey: string } | null } | { ok: false; reason: string }
> {
  const signIn = anchor.surface.signIn?.bind(anchor.surface);
  const openShareScreen = anchor.surface.openShareScreen?.bind(anchor.surface);
  const readShare = anchor.surface.readShare?.bind(anchor.surface);
  if (!signIn || !openShareScreen || !readShare) {
    return {
      ok: false,
      reason:
        `${anchor.surface.name} cannot sign in and open the share screen ` +
        `(signIn=${Boolean(signIn)} openShareScreen=${Boolean(openShareScreen)} readShare=${Boolean(readShare)})`,
    };
  }
  if (!canJoin(joiner)) {
    return {
      ok: false,
      reason: `${joiner.surface.name} cannot join an account (no openPair helper, no env-able relaunch with a deep-link channel)`,
    };
  }
  await signIn(anchor.peer, username, PASSWORD, budget);
  await openShareScreen(anchor.peer, PASSWORD, budget);
  const share = await readShare(anchor.peer, budget);
  ctx.record({
    surface: anchor.surface.name,
    peer: anchor.peer,
    kind: 'log',
    label: 'pairing share (same account)',
    line: ctx.mask(`pin=${share.pin} deep_link=${share.deepLink}`),
  });
  // The identity marker is what a native anchor's whole identity flow lands
  // on; pairing pulls the account record over DHT, so it belongs in evidence.
  // Returned because the paired device holds this account from here on: a
  // scenario that wants to talk to it needs the account's username.
  const anchorIdentity = await recordIdentity(ctx, anchor);
  await joinPairedDevice(ctx, joiner, share, PASSWORD, budget);
  const waitComplete = joiner.surface.waitComplete?.bind(joiner.surface);
  if (waitComplete) await waitComplete(joiner.peer, budget);
  return { ok: true, anchorIdentity };
}

/** The pairing share URL the app encodes into the pairing QR. */
export function pairingShareUrl(deepLink: string): string {
  const payload = deepLink.split('data=')[1]?.split('#')[0] ?? '';
  return `https://oo-app.pages.dev/p/${payload}`;
}

