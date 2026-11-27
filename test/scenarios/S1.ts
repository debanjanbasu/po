// S1 — pairing: anchor generates the share (event=share seam), joiner pairs
// with the PIN + account password and reaches "Device sync complete".
//
// Observable (docs/device-testing.md S1): the joiner's PAIRING_SYNC_STATUS
// reaches Complete. There is no `event=` marker for that stage yet (P2 seam
// work), so we ride the UI-observable proxy: on web, DOM text; on native
// surfaces, the beacon-visible share side plus a generous settle, since the
// only e2e marker today is the anchor's `event=share`.
import { SeamMissing } from '../lib/wait.ts';
import { findMarker, hasAnyMarker, waitFor, waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { assertDhtReady, joinPairedDevice, recordIdentity, recordObservation } from './support.ts';

export const name = 'S1';
export const description =
  'Pairing: anchor shares (event=share), joiner prefills PIN+password and completes device sync';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  const anchor = surfaces[0];
  const joiner = surfaces[1] ?? anchor;
  const peers = { anchor: anchor.peerIds[0], joiner: joiner.peerIds[joiner.peerIds.length - 1] };
  const budget = 120_000;
  const password = 'E2e-Passw0rd!x';

  // 1. Anchor: sign in and open the share screen. On the web surface this is
  //    driven through the DOM; native surfaces have no UI driver (no typing),
  //    so the full pairing cannot run yet — instead run the same-host Veilid
  //    discovery probe and report the exact blocker.
  if (!anchor.signIn || !anchor.openShareScreen || !anchor.readShare) {
    const observed = await discoveryProbe(surfaces, 90_000);
    throw new Error(
      'S1 pairing blocked on native surfaces: the anchor must sign in and type its account password on the ' +
        'share screen, but macOS/iOS-sim drivers have no typing channel (docs P3). The app-side ' +
        '`event=share` seam exists only after a signed-in share; no launch-time or env seam generates one. ' +
        `Same-host discovery probe: ${observed}`,
    );
  }
  await anchor.signIn(peers.anchor, 'e2e-anchor', password, budget);
  await anchor.openShareScreen(peers.anchor, password, budget);

  // 2. Anchor: generate the share (PIN + deep link).
  const share = await anchor.readShare(peers.anchor, budget);
  ctx.record({
    surface: anchor.name,
    peer: peers.anchor,
    kind: 'log',
    label: 'pairing share',
    line: ctx.mask(`pin=${share.pin} deep_link=${share.deepLink}`),
  });
  // The joiner's pairing sync pulls the anchor's account identity over DHT;
  // the anchor's identity marker is part of that path's evidence.
  await recordIdentity(ctx, {
    surface: anchor,
    peer: peers.anchor,
  });

  // 3. Joiner: get it into the account — the pairing form on a surface with a
  //    DOM, the launch environment plus a deep link on a native one. Shared
  //    with every other scenario that pairs a joiner (support.ts).
  await joinPairedDevice(ctx, { surface: joiner, peer: peers.joiner }, share, password, budget);

  // 4. Joiner: wait for completion. Marker if the seam exists; DOM text on
  //    web ("Device sync complete" banner label from PairingSyncStage::Complete).
  // The pairing sync cannot start before the joiner's DHT gate opened
  // (src/user/pairing.rs waits on it); assert the gate itself so a lapsed
  // wait fails here as "the DHT never came ready" instead of as a
  // downstream pairing timeout.
  await assertDhtReady(ctx, { surface: joiner, peer: peers.joiner }, budget);
  try {
    if (joiner.waitComplete) {
      await joiner.waitComplete(peers.joiner, budget);
    } else {
      await waitForSignal(
        ctx,
        joiner,
        peers.joiner,
        { label: 'pairing complete', marker: 'paired' },
        budget,
      );
    }
    ctx.record({
      surface: joiner.name,
      peer: peers.joiner,
      kind: 'log',
      label: 'pairing complete',
      line: 'joiner reached device-sync-complete state',
    });
    // Negative assertion (docs/device-testing.md S1 row): a clean pairing
    // surfaces no `auth.pairing` background error. `report_nonfatal` has no
    // event marker — it logs a `WARN` whose message carries the domain tag
    // (e.g. "Failed to enqueue pre-pairing sync op: …") — so match the log
    // line, which is what a human debugging would grep for.
    const logText = joiner.logs(peers.joiner);
    const pairingError = logText
      .split('\n')
      .find((line) =>
        /WARN.*(Failed to enqueue pre-pairing sync op|Pairing failed|Failed to generate pairing payload|Failed to finalize paired account)/u.test(
          line,
        ),
      );
    if (pairingError !== undefined) {
      throw new Error(`joiner logged an auth.pairing background error: ${ctx.mask(pairingError)}`);
    }
    recordObservation(
      ctx,
      { surface: joiner, peer: peers.joiner },
      'no auth.pairing background errors',
      'no auth.pairing WARN line in the joiner log after pairing completed',
    );
  } catch (err) {
    const text = joiner.logs(peers.joiner);
    if (
      hasAnyMarker(text) &&
      findMarker(text, 'paired') === undefined &&
      findMarker(text, 'share') === undefined
    ) {
      // The `paired` seam exists and fires on a completed sync, so markers
      // without it mean the post-pairing sync did not finish — not a seam
      // gap. The historical cause was a cold-routing-table `TryAgain` on the
      // identity pull, which aborted the sync before the marker; see
      // docs/dht-architecture.md § Cold-Routing-Table Subkey Reads.
      throw new SeamMissing(
        `joiner produced event= lines but never reached the pairing-complete marker: ` +
          `the post-pairing sync did not finish (${String(err)})`,
      );
    }
    throw err;
  }

  ctx.note(`S1 OK on ${anchor.name}(anchor)+${joiner.name}(joiner)`);
}

/**
 * Same-host discovery probe: do two PO_INSTANCE processes learn each other?
 *
 * `Network state: … peers=N` is Veilid's "most recently accessed peers" list,
 * not the routing table, so it stays 0 on an idle node — it is not a discovery
 * signal. The honest signal is cross-reference: does one peer's log ever name
 * the other peer's node id?
 */
export async function discoveryProbe(surfaces: Surface[], settleMs: number): Promise<string> {
  const nodeIdOf = (text: string): string | undefined =>
    text.match(/Node Ids: \[(VLD0:[A-Za-z0-9_-]+)\]/u)?.[1];
  // Declared outside the probe: the timeout path below reports the last
  // snapshot, and a `const` inside the probe closure is out of scope there.
  let peers: PeerSnapshot[] = [];
  try {
    const learned = await waitFor(
      () => {
        peers = surfaces.flatMap((s) =>
          s.peerIds.map((p) => ({
            label: `${s.name}/${p}`,
            id: nodeIdOf(s.logs(p)),
            text: s.logs(p),
          })),
        );
        const found = peers.flatMap((a) =>
          peers
            .filter((b) => b.label !== a.label && b.id !== undefined && a.text.includes(b.id))
            .map((b) => `${a.label} learned ${b.label}`),
        );
        return found.length > 0 ? found : undefined;
      },
      { label: 'cross-peer discovery', timeoutMs: settleMs, intervalMs: 2000 },
    );
    return learned.join('; ');
  } catch {
    const ids = peers.map((p) => `${p.label}: ${p.id ?? 'no node id logged'}`).join(' | ');
    return `no peer learned the other's node id within ${settleMs}ms (${ids})`;
  }
}

/** One peer's observable state, for the discovery cross-reference. */
interface PeerSnapshot {
  label: string;
  id: string | undefined;
  text: string;
}
