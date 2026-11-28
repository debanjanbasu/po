// S9 — negatives.
//
// Observables (docs/device-testing.md § S9):
//   1. wrong PIN → `event=pairing_rejected reason=invalid_credentials`, and
//      the repeated wrong PIN reaches the lockout threshold
//      (`event=pairing_rejected reason=cooldown`)
//   2. expired payload TTL → `event=pairing_rejected reason=expired`
//   3. a pairing link opened on an already-signed-in device → the app refuses
//      it and no admission happens
//
// Cases 1 and 2 run on the joiner (signed out) side, which needs the anchor's
// pairing form. Case 3 runs on the anchor (signed in) side.
//
// Honest gap in case 3: there is NO `pairing_rejected` marker for it. The app
// refuses a pairing link on a signed-in device in `NavLayout` BEFORE any
// pairing attempt exists (the deep link is consumed, never turned into an
// attempt), so there is nothing to reject and the shared marker vocabulary has
// no name for it. The web-observable equivalent of the refusal is the pairing
// ROUTE on a signed-in device: the app renders the SHARE side and never the
// joiner form, so it cannot admit anybody. On native the refusal is a 5-second
// toast ("Pairing links only work on a device that isn't signed in") with no
// marker, so it is observable by screenshot only.
import { findMarker, sleep } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { BUDGET, PASSWORD, pickPeers, recordObservation, skip, waitMarkerField } from './support.ts';

export const name = 'S9';
export const description =
  'Negatives: wrong PIN (and its cooldown), expired payload TTL, and a pairing link on a signed-in device';

/**
 * The payload lifetime this scenario runs with, in seconds.
 *
 * The app's production value is five minutes, and waiting that out was the
 * suite's single largest fixed cost — a static sleep, in a harness that
 * otherwise waits on conditions. `PO_E2E_PAIRING_TTL_SECS` replaces it with the
 * value below, read by the *creating* side only (the payload carries its own
 * `expires_at`), so the app still takes its real expiry path: a genuine,
 * expired payload rather than a simulated one. The seam is compiled out of
 * every non-harness build (`src/config/e2e.rs`).
 */
const TTL_SECS = 15;
export const seams = { PO_E2E_PAIRING_TTL_SECS: String(TTL_SECS) };

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  const { first: signedIn, second: joiner } = pickPeers(surfaces);

  const missing: string[] = [];
  if (!signedIn.surface.signIn || !signedIn.surface.openShareScreen || !signedIn.surface.readShare) {
    missing.push(`${signedIn.surface.name}: signIn/openShareScreen/readShare`);
  }
  if (!joiner.surface.openPair) missing.push(`${joiner.surface.name}: openPair`);
  if (missing.length > 0) {
    skip(ctx, signedIn, 'negative-case capabilities', missing.join('; '));
    return;
  }

  // The anchor: a signed-in account that can produce a share. It never runs
  // the pairing path, so there is no `dht_ready` to assert here
  // (support.ts::assertDhtReady); the share screen's own QR is the observable.
  await signedIn.surface.signIn?.(signedIn.peer, 'e2e-s9-anchor', PASSWORD, BUDGET);
  await signedIn.surface.openShareScreen?.(signedIn.peer, PASSWORD, BUDGET);
  const share = await signedIn.surface.readShare?.(signedIn.peer, BUDGET);
  if (!share) {
    skip(ctx, signedIn, 'readShare', `${signedIn.surface.name} could not read the share`);
    return;
  }
  ctx.record({
    surface: signedIn.surface.name,
    peer: signedIn.peer,
    kind: 'log',
    label: 'share for the negative cases',
    line: ctx.mask(`pin=${share.pin} deep_link=${share.deepLink}`),
  });

  // ── Case 1: wrong PIN ────────────────────────────────────────────────────
  // A 12-digit PIN that is not the share's PIN: the payload does not decrypt,
  // so the app rejects the credentials. The attempt counter is durable and
  // keyed on the payload, so the retries reach the lockout threshold and the
  // app takes the cooldown path.
  const wrongPin = share.pin === '000000000000' ? '111111111111' : '000000000000';
  await joiner.surface.openPair?.(joiner.peer, share.deepLink, wrongPin, PASSWORD, BUDGET);
  await waitMarkerField(
    ctx,
    joiner,
    'pairing_rejected',
    'reason',
    'invalid_credentials',
    'pairing rejected (wrong PIN)',
    BUDGET,
  );
  await waitMarkerField(
    ctx,
    joiner,
    'pairing_rejected',
    'reason',
    'cooldown',
    'pairing rejected (cooldown)',
    BUDGET,
  );
  ctx.record({
    surface: joiner.surface.name,
    peer: joiner.peer,
    kind: 'log',
    label: 'wrong PIN rejected, then locked out',
    line: 'reason=invalid_credentials then reason=cooldown',
  });

  // Nothing was admitted by the failed attempts.
  const pairedOnJoiner = findMarker(joiner.surface.logs(joiner.peer), 'paired');
  if (pairedOnJoiner !== undefined) {
    throw new Error(
      `the joiner was admitted despite the rejected credentials: ${ctx.mask(pairedOnJoiner)}`,
    );
  }
  recordObservation(ctx, joiner, 'no admission after the wrong-PIN cases', 'event=paired absent');

  // ── Case 2: expired payload TTL ─────────────────────────────────────────
  // The case waits the TTL out and then submits the CORRECT credentials: the
  // payload decrypts and is then found expired. `seams` above shortened that TTL
  // for this run, so the wait is a real expiry in seconds rather than a static
  // five minutes — the assertion is unchanged, only the clock it needs.
  const ttlMs = TTL_SECS * 1_000;
  const expiryBudget = ttlMs + 180_000;
  ctx.note(
    `S9: waiting out the ${ttlMs / 1000}s pairing TTL before the expired-payload case`,
  );
  await sleep(ttlMs + 2_000);
  await joiner.surface.openPair?.(joiner.peer, share.deepLink, share.pin, PASSWORD, expiryBudget);
  await waitMarkerField(
    ctx,
    joiner,
    'pairing_rejected',
    'reason',
    'expired',
    'pairing rejected (expired payload)',
    expiryBudget,
  );
  ctx.record({
    surface: joiner.surface.name,
    peer: joiner.peer,
    kind: 'log',
    label: 'expired payload rejected',
    line: 'reason=expired, no admission',
  });

  const pairedAfterExpiry = findMarker(joiner.surface.logs(joiner.peer), 'paired');
  if (pairedAfterExpiry !== undefined) {
    throw new Error(
      `the joiner was admitted with an expired payload: ${ctx.mask(pairedAfterExpiry)}`,
    );
  }
  recordObservation(ctx, joiner, 'no admission with an expired payload', 'event=paired absent');

  // ── Case 3: pairing link on an already-signed-in device ──────────────────
  // The pairing route on a signed-in device renders the SHARE side (the
  // device's own share) and never the joiner form, so it cannot admit a new
  // device, and the device stays signed in as its own account.
  const openPairing = signedIn.surface.openPairing?.bind(signedIn.surface);
  const readText = signedIn.surface.readText?.bind(signedIn.surface);
  if (!openPairing || !readText) {
    skip(
      ctx,
      signedIn,
      'openPairing/readText',
      `${signedIn.surface.name} cannot show the pairing route to a signed-in device`,
    );
    return;
  }
  await openPairing(signedIn.peer, PASSWORD, BUDGET);
  const text = await readText(signedIn.peer);
  // The share side renders the QR card; the joiner form renders a PIN field.
  // "Required to encrypt pairing payload" is the password gate's hint and is
  // gone once the gate is unlocked, so it cannot identify the share side.
  if (!text.includes('Pairing QR')) {
    throw new Error(
      'the pairing route on a signed-in device did not render the share side: ' +
        ctx.mask(text.slice(0, 300)),
    );
  }
  if (text.includes('Pairing PIN') || text.includes('Pairing payload')) {
    throw new Error(
      'the pairing route on a signed-in device rendered the joiner form — it could admit a new device',
    );
  }
  const pairedOnSignedIn = findMarker(signedIn.surface.logs(signedIn.peer), 'paired');
  if (pairedOnSignedIn !== undefined) {
    throw new Error(
      `the signed-in device was admitted into another account: ${ctx.mask(pairedOnSignedIn)}`,
    );
  }
  recordObservation(
    ctx,
    signedIn,
    'no admission on the signed-in device',
    'event=paired absent, the share side rendered',
  );
  ctx.record({
    surface: signedIn.surface.name,
    peer: signedIn.peer,
    kind: 'log',
    label: 'pairing link refused on a signed-in device',
    line: 'the share side is rendered, no joiner form, no admission',
  });

  ctx.note(
    `S9 OK on ${signedIn.surface.name}/${signedIn.peer} + ${joiner.surface.name}/${joiner.peer}: ` +
      'wrong-PIN/cooldown, expired and signed-in refusals all rejected without admission',
  );
}
