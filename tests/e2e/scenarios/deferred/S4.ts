// S4 — profile conflict.
//
// Observables (docs/device-testing.md § S4): `event=profile_merged field=dob`
// on the device whose local profile was merged, and (on a surface with a DOM)
// the DOB-conflict row on the device that held an unsaved conflicting edit.
//
// The profile lives on the ACCOUNT's identity record, so the two peers must be
// TWO DEVICES OF THE SAME ACCOUNT: B pairs into A's account (the S1 flow),
// then B edits the DOB while A holds an unsaved conflicting edit. A's identity
// watch pulls B's change and merges it; A keeps its unsaved edit and shows
// the conflict row.
import { waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { BUDGET, assertDhtReady, pairSecondDevice, pickPeers, recordObservation, skip } from './support.ts';

export const name = 'S4';
export const description =
  'Profile conflict: a remote DOB change merges into the local profile while an unsaved edit is held';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  const { first: alpha, second: beta } = pickPeers(surfaces);

  const missing: string[] = [];
  if (!alpha.surface.signIn || !alpha.surface.openShareScreen || !alpha.surface.readShare) {
    missing.push(`${alpha.surface.name}: signIn/openShareScreen/readShare`);
  }
  if (!beta.surface.openPair) {
    missing.push(`${beta.surface.name}: openPair`);
  }
  if (!beta.surface.editProfile) {
    missing.push(`${beta.surface.name}: editProfile`);
  }
  if (!alpha.surface.stageProfileEdit) {
    missing.push(`${alpha.surface.name}: stageProfileEdit`);
  }
  if (missing.length > 0) {
    skip(ctx, alpha, 'profile-conflict capabilities', missing.join('; '));
    return;
  }

  // Two devices of the SAME account — the profile is per account.
  const paired = await pairSecondDevice(ctx, alpha, beta, 'e2e-s4-account', BUDGET);
  if (!paired.ok) {
    skip(ctx, alpha, 'same-account pair', paired.reason);
    return;
  }
  // B ran the joining path, and that is the only path that emits `dht_ready`
  // (support.ts::assertDhtReady) — so the gate is asserted on the PAIRED
  // device, after the pairing completed, and not on A, which only signs in.
  // A's own attachment is covered by the merge assertion below, which cannot
  // happen before A's gate is open.
  await assertDhtReady(ctx, beta, BUDGET);

  // A holds an unsaved conflicting edit: the DOB field is filled but neither
  // confirmed nor saved, so the app keeps it dirty.
  await alpha.surface.stageProfileEdit?.(alpha.peer, '1991-02-03', BUDGET);
  ctx.record({
    surface: alpha.surface.name,
    peer: alpha.peer,
    kind: 'log',
    label: 'unsaved DOB edit staged',
    line: 'dob=1991-02-03 (not confirmed, not saved)',
  });

  // B edits the same account's DOB and saves it.
  await beta.surface.editProfile?.(beta.peer, '1988-04-05', BUDGET);
  ctx.record({
    surface: beta.surface.name,
    peer: beta.peer,
    kind: 'log',
    label: 'DOB edited on the second device',
    line: 'dob=1988-04-05 saved',
  });

  // A's identity watch pulls the change and merges the field.
  const merged = await waitForSignal(
    ctx,
    alpha.surface,
    alpha.peer,
    { label: 'profile merged (alpha)', marker: 'profile_merged' },
    BUDGET,
  );
  const field = merged.fields['field'];
  if (field !== 'dob') {
    throw new Error(
      `event=profile_merged names another field: ${ctx.mask(field ?? '(none)')} (expected dob)`,
    );
  }
  // Pinned, not just matched: the `field=dob` assertion itself is evidence.
  recordObservation(ctx, alpha, 'profile merged (field=dob)', merged.line);

  // The unsaved edit survives and the app shows the conflict row.
  const readProfileConflict = alpha.surface.readProfileConflict?.bind(alpha.surface);
  if (readProfileConflict) {
    const conflict = await readProfileConflict(alpha.peer);
    if (conflict === null) {
      throw new Error(
        'the DOB-conflict row is not rendered even though the local edit was dirty and the remote DOB merged',
      );
    }
    ctx.record({
      surface: alpha.surface.name,
      peer: alpha.peer,
      kind: 'log',
      label: 'DOB conflict row',
      line: conflict,
    });
  } else {
    skip(
      ctx,
      alpha,
      'readProfileConflict',
      `${alpha.surface.name} has no DOM to show the DOB-conflict row`,
    );
  }

  ctx.note(
    `S4 OK on ${alpha.surface.name}/${alpha.peer} + ${beta.surface.name}/${beta.peer}: ` +
      'the remote DOB merged and the unsaved local edit raised a conflict',
  );
}
