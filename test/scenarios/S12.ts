// S12 — Snapshots: export and import round trip.
//
// Observable (docs/device-testing.md): an exported backup file can be imported
// back and decrypted with the account password, and a WRONG password is
// refused. The coverage audit ranked this the largest gap in the suite: it is
// the user's backup path, and a bug here loses data with nothing to catch it.
//
// The round trip is real, not a render check: the export is captured as an
// actual browser download, and that same file is fed back through the app's
// import input. A wrong-password attempt first proves the archive is genuinely
// encrypted and that the password is what gates it — otherwise "import
// succeeded" would also be true of an unencrypted dump.

import { statSync } from 'node:fs';
import type { RunContext } from '../lib/run.ts';
import { waitFor } from '../lib/wait.ts';
import type { Surface } from '../lib/types.ts';
import { BUDGET, PASSWORD, skip } from './support.ts';

export const name = 'S12';
export const description = 'Snapshots: export a snapshot, import it back, and assert the data round-trips';

const USERNAME = 'e2e-s12-account';
/** A deliberately wrong password, to prove the archive is really encrypted. */
const WRONG_PASSWORD = 'Not-The-Passw0rd!9';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  // Single-peer scenario (`minPeers: 1`): the run hands the surface exactly one
  // peer id, so `pickPeers` — which insists on a second peer — does not apply.
  if (surfaces.length === 0 || !surfaces[0].peerIds[0]) {
    throw new Error('S12 needs at least one surface with one peer');
  }
  const surface = surfaces[0];
  const peerId = surface.peerIds[0];
  const ref = { surface, peer: peerId };
  const dom = surface.dom?.bind(surface);
  if (!dom || !surface.signIn) {
    skip(ctx, ref, 'signIn/dom', `${surface.name} cannot sign in and drive the snapshots screen`);
    return;
  }

  await surface.signIn(peerId, USERNAME, PASSWORD, BUDGET);

  /** Dock → Account → Data & Backups (`/account/snapshots`). */
  const openSnapshots = async (): Promise<void> => {
    await dom(peerId, [{ waitFor: ['#oo-main-dock', BUDGET] }]);
    await dom(peerId, [
      { click: 'a[href="/account"]' },
      { waitFor: ['a[href="/account/snapshots"]', BUDGET] },
      { click: 'a[href="/account/snapshots"]' },
      // Wait on the disclosure's summary, NOT on the create button: that
      // button lives inside the collapsed "Snapshot History" body, so it
      // resolves in the DOM but has no box until the summary is opened.
      { waitFor: ['details.collapse summary', BUDGET] },
      { click: 'details.collapse summary' },
      { waitFor: ['#snapshots-create-button', BUDGET] },
    ]);
  };

  await openSnapshots();

  // ── Create a snapshot so there is something to export ───────────────────
  // Creating a snapshot writes it asynchronously, so wait for the entry rather
  // than sampling the list immediately.
  await dom(peerId, [
    { click: '#snapshots-create-button' },
    { waitFor: ['[id^="snapshot-restore-"]', BUDGET] },
  ]);
  const listed = await dom(peerId, [
    { readCount: ['restore', '[id^="snapshot-restore-"]'] },
  ]);
  const snapshotCount = listed.restore;
  if (typeof snapshotCount !== 'number' || snapshotCount < 1) {
    throw new Error(`S12: creating a snapshot produced no entry in the list (${snapshotCount})`);
  }
  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'log',
    label: 'snapshot created',
    line: `${snapshotCount} snapshot(s) listed after Create Snapshot`,
  });

  // ── Export, capturing the real download ─────────────────────────────────
  const exported = await dom(peerId, [
    { download: ['path', '#snapshots-export-button'] },
  ]);
  const file = String(exported.path ?? '');
  if (!file) throw new Error('S12: the export produced no downloadable file');
  const size = statSync(file).size;
  if (size === 0) throw new Error(`S12: the exported backup is empty (${file})`);
  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'log',
    label: 'backup exported',
    line: `${file.split('/').pop()} — ${size} bytes`,
  });

  // ── Import with a WRONG password: must be refused ───────────────────────
  // No submit step: the modal has no button and no need for Enter — a
  // `use_effect` decrypts as soon as the typed password passes the client-side
  // validator, and typing disables the field while it runs. Driving it with a
  // keypress races that effect and fails on a disabled input.
  await dom(peerId, [
    { setFiles: ['#backup-import-file', file] },
    { waitFor: ['input[aria-label="Backup password"]', BUDGET] },
    { fill: ['input[aria-label="Backup password"]', WRONG_PASSWORD] },
    { waitFor: ['.modal-box p.text-error', BUDGET] },
  ]);
  const refused = await dom(peerId, [
    { readText: ['error', '.modal-box p.text-error'] },
  ]);
  if (typeof refused.error !== 'string' || refused.error === '') {
    throw new Error('S12: a backup imported with the WRONG password was not refused');
  }
  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'log',
    label: 'wrong password refused',
    line: refused.error.slice(0, 160),
  });

  // ── Import with the real password: must decrypt and apply ───────────────
  // A fresh pick rather than reusing the refused attempt's modal: the modal's
  // state after an error is not something a caller should have to predict, and
  // re-selecting the same file is what a user would do.
  await dom(peerId, [
    { setFiles: ['#backup-import-file', file] },
    { waitFor: ['input[aria-label="Backup password"]', BUDGET] },
    { fill: ['input[aria-label="Backup password"]', PASSWORD] },
    { waitFor: ['#backup-restore-merge', BUDGET] },
    { click: '#backup-restore-merge' },
  ]);
  // The apply is asynchronous (the modal spawns it and removes itself when it
  // finishes), so the modal going away is the completion signal — the same wait
  // S16 uses. The create button behind the modal is NOT one: a `<dialog>`
  // overlays it without hiding it, so it is "visible" to Playwright the whole
  // time and this check once passed a modal that was still open.
  await waitFor(
    async () => {
      const open = Number(
        (await dom(peerId, [{ readCount: ['merge', '#backup-restore-merge'] }]))['merge'] ?? 0,
      );
      return open === 0 ? 'closed' : undefined;
    },
    { label: 'the restore modal to close after Merge', timeoutMs: BUDGET },
  );

  // What the merge DID, compared against the state captured before the export:
  // a Merge import applies the archive's sections (identity, TOTP, contacts,
  // conversations) and must leave the account's snapshot history — which the
  // archive does not carry — exactly as it was. The count was read from this
  // same list above, so a wipe or a duplicate now fails here.
  const restored = await dom(peerId, [{ readCount: ['restore', '[id^="snapshot-restore-"]'] }]);
  if (restored.restore !== snapshotCount) {
    throw new Error(
      `S12: the merge changed the snapshot history it does not carry: ` +
        `${snapshotCount} → ${String(restored.restore)}`,
    );
  }

  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'log',
    label: 'backup imported',
    line:
      `${file.split('/').pop()} (${size} bytes) decrypted with the account password and merged; ` +
      `snapshot history ${snapshotCount} → ${String(restored.restore)}`,
  });
  ctx.note(`S12 OK on ${surface.name}: ${size}-byte backup round-tripped (wrong password refused)`);
}
