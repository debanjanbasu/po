// S7 — dictionary sync.
//
// Observables (docs/device-testing.md § S7): `event=dict_synced digest=<b3:…>`
// on BOTH sides, and the two digests are the same content identity.
//
// The dictionary lives on the ACCOUNT's identity record, so the two peers must
// be TWO DEVICES OF THE SAME ACCOUNT: B pairs into A's account, then the
// dictionary metadata is pulled and both devices converge on one digest.
import { waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { BUDGET, countMarker, pairSecondDevice, pickPeers, skip, waitMarkerCount } from './support.ts';

export const name = 'S7';
export const description =
  'Dictionary sync: both devices of one account report the same b3:<digest>';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  const { first: alpha, second: beta } = pickPeers(surfaces);

  const missing: string[] = [];
  if (!alpha.surface.signIn || !alpha.surface.openShareScreen || !alpha.surface.readShare) {
    missing.push(`${alpha.surface.name}: signIn/openShareScreen/readShare`);
  }
  if (!beta.surface.openPair) missing.push(`${beta.surface.name}: openPair`);
  if (missing.length > 0) {
    skip(ctx, alpha, 'dictionary-sync capabilities', missing.join('; '));
    return;
  }

  // Counted before the pairing, because `event=dict_synced` is reported by
  // BOTH halves of the sync — the announcing side (`OpType::DictPublish`,
  // which a signing-in device runs) and the receiving side
  // (`dict_sync::check_and_sync`, which the joining device runs during the
  // pairing sync). A line that was there before the pairing proves nothing
  // about it.
  const betaSyncedBefore = countMarker(beta.surface.logs(beta.peer), 'dict_synced');

  // Two devices of the SAME account — the dictionary is published on the
  // account's identity record.
  const paired = await pairSecondDevice(ctx, alpha, beta, 'e2e-s7-account', BUDGET);
  if (!paired.ok) {
    skip(ctx, alpha, 'same-account pair', paired.reason);
    return;
  }

  // The joining device syncs the dictionary as part of its pairing sync
  // (`user::pairing` runs `dict_sync::check_and_sync` before Complete): it
  // reads the account's announced dictionary and reports the content id it
  // converged on. Required to be a NEW line on the joiner — that is the half
  // of the claim the pairing can make or break. The anchor's own line is the
  // announcement (`DictPublish`), so it is read as-is.
  //
  // Both devices ship the same compiled-in dictionary, so the two digests
  // being equal is not by itself evidence of convergence; the seam cannot say
  // which half fired (`report_dict_converged`, src/compression/codec/
  // registry.rs, reports the remote version identically for "already active"
  // and "just adopted"). A `via=publish|adopt` field would let the joiner's
  // adoption be asserted outright.
  const alphaSync = await waitForSignal(
    ctx,
    alpha.surface,
    alpha.peer,
    { label: 'dictionary synced (alpha)', marker: 'dict_synced' },
    BUDGET,
  );
  const betaSync = await waitMarkerCount(
    ctx,
    beta,
    'dict_synced',
    betaSyncedBefore + 1,
    'dictionary synced (joining device)',
    BUDGET,
  );

  const alphaDigest = alphaSync.fields['digest'];
  const betaDigest = betaSync.fields['digest'];
  if (!alphaDigest || !alphaDigest.startsWith('b3:')) {
    throw new Error(
      `event=dict_synced on ${alpha.surface.name} carries no b3: digest: ${ctx.mask(alphaSync.line)}`,
    );
  }
  if (alphaDigest !== betaDigest) {
    throw new Error(
      `the two devices report different dictionaries\n` +
        `  ${alpha.surface.name}: ${ctx.mask(alphaDigest)}\n` +
        `  ${beta.surface.name}: ${ctx.mask(betaDigest)}`,
    );
  }
  ctx.record({
    surface: beta.surface.name,
    peer: beta.peer,
    kind: 'log',
    label: 'dictionary digest converged',
    line:
      `digest=${ctx.mask(betaDigest)} on both devices; the joining device reported it ` +
      `${betaSyncedBefore} → ${betaSyncedBefore + 1} time(s) across the pairing`,
  });

  ctx.note(
    `S7 OK on ${alpha.surface.name}/${alpha.peer} + ${beta.surface.name}/${beta.peer}: ` +
      `both report ${alphaDigest}`,
  );
}
