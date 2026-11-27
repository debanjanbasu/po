// S16 — Snapshots: Replace and Merge are different restores.
//
// S12 covers the export → import round trip and the wrong-password refusal, but
// it only ever presses *Merge*. The two buttons promise opposite things:
//
//   Replace — "Overwrites current data entirely with the backup."
//   Merge   — "Adds missing backup items and keeps current values for existing items."
//
// Nothing checked that distinction, and Replace is the destructive branch: if
// it behaved like Merge, a user restoring an old backup would silently keep
// everything they believed the restore had rolled back. Or if the restore
// failed closed and applied nothing, "it did not corrupt anything" would look
// like a pass — which is why this scenario imports the SAME file twice:
//
//   1. export a backup of a state with no TOTP tokens,
//   2. add a token AFTER the export,
//   3. Merge that file   → the token must SURVIVE (merge keeps current items),
//   4. Replace that file → the token must be GONE (replace overwrites).
//
// Both directions are asserted against the same observable, so neither
// "restore did nothing" nor "replace is a merge" can pass.
//
// `replace_local` (`src/storage/store_trait/mod.rs`) is what makes step 4 hold
// across devices: the replace writes a causally-after CRDT delete, so a later
// DHT merge cannot resurrect the token. A failure here is a real finding (the
// delete did not stick) — not a test artefact.

import { statSync } from 'node:fs';
import { waitFor } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { BUDGET, PASSWORD, skip } from './support.ts';

export const name = 'S16';
export const description =
  'Snapshots: Merge keeps a post-export change, Replace discards it (the destructive restore branch)';

const USERNAME = 'e2e-s16-account';
/**
 * A valid `otpauth://` URI is the only input the TOTP screen has (see S10).
 *
 * The seed is S10's: the RFC 6238 Appendix B secret (ASCII
 * "12345678901234567890", 128 bits). The RFC 4226 example
 * `JBSWY3DPEHPK3PXP` is only 80 bits and `src/components/features/totp.rs`
 * parses through `totp_rs`, which refuses anything under 128 bits — the app
 * would show its own validation error ("MUST be at least 128 bits, got 80")
 * and this scenario would never reach its Merge/Replace assertions.
 */
const TOTP_URI =
  'otpauth://totp/E2E:s16?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=E2E&algorithm=SHA1&digits=6&period=30';
/** Snapshot entries the history lists once one exists (`snapshot-restore-<id>`). */
const SNAPSHOT_ENTRY = '[id^="snapshot-restore-"]';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  // Single-peer scenario (`minPeers: 1`): the run hands the surface exactly one
  // peer id, so `pickPeers` — which insists on a second peer — does not apply.
  if (surfaces.length === 0 || !surfaces[0].peerIds[0]) {
    throw new Error('S16 needs at least one surface with one peer');
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
    await dom(peerId, [{ waitFor: ['#po-main-dock', BUDGET] }]);
    await dom(peerId, [
      { click: '#po-dock-Account' },
      { waitFor: ['#settings-link-snapshots', BUDGET] },
      { click: '#settings-link-snapshots' },
      // The create button lives inside the collapsed "Snapshot History" body,
      // so it resolves in the DOM but has no box until the summary is opened.
      { waitFor: ['details.collapse summary', BUDGET] },
      { click: 'details.collapse summary' },
      { waitFor: ['#snapshots-create-button', BUDGET] },
    ]);
  };

  /**
   * Dock → Account → Data & Backups, without touching the snapshot history
   * disclosure: the export/import cards sit outside it, and a second click on
   * the summary would CLOSE an already-open disclosure.
   */
  const gotoSnapshots = async (): Promise<void> => {
    await dom(peerId, [
      { click: '#po-dock-Account' },
      { waitFor: ['#settings-link-snapshots', BUDGET] },
      { click: '#settings-link-snapshots' },
      { waitFor: ['#snapshots-export-button', BUDGET] },
    ]);
  };

  /** Dock → Authenticator, where the post-export change is made and read. */
  const openTotp = async (): Promise<void> => {
    await dom(peerId, [
      { click: '#po-dock-Totp' },
      { waitFor: ['#totp-uri-input', BUDGET] },
    ]);
  };

  /**
   * Import `file` and press `mode`, then wait for the restore modal to be gone.
   *
   * The file input is `sr-only`, so only `setFiles` can reach it. The password
   * modal decrypts on its own `use_effect` — there is no submit button, and a
   * keypress races the disabled-while-decrypting field (S12 measured this).
   */
  const importAndApply = async (file: string, mode: 'merge' | 'replace'): Promise<void> => {
    await gotoSnapshots();
    await dom(peerId, [
      { setFiles: ['#backup-import-file', file] },
      { waitFor: ['input[aria-label="Backup password"]', BUDGET] },
      { fill: ['input[aria-label="Backup password"]', PASSWORD] },
      { waitFor: [`#backup-restore-${mode}`, BUDGET] },
      { click: `#backup-restore-${mode}` },
    ]);
    await waitFor(
      async () => {
        const open = Number(
          (await dom(peerId, [{ readCount: ['modes', `#backup-restore-${mode}`] }]))['modes'] ?? 0,
        );
        return open === 0 ? 'closed' : undefined;
      },
      { label: `the ${mode} restore modal to close`, timeoutMs: BUDGET },
    );
  };

  /**
   * The TOTP screen's two mutually exclusive states: a rendered entry or its
   * empty state. Read to a settled answer — the code is computed
   * asynchronously after the token is stored, so a bare count races it.
   */
  const totpState = async (): Promise<{ codes: number; empty: number }> => {
    const settled = await waitFor(
      async () => {
        const out = await dom(peerId, [
          { readCount: ['codes', '#totp-code-0'] },
          { readCount: ['empty', '#totp-empty-state'] },
        ]);
        const codes = Number(out['codes'] ?? 0);
        const empty = Number(out['empty'] ?? 0);
        return codes + empty > 0 ? { codes, empty } : undefined;
      },
      { label: 'the TOTP screen to settle', timeoutMs: BUDGET },
    );
    return settled;
  };

  // ── A backup of a state with no TOTP tokens ─────────────────────────────
  await openSnapshots();
  await dom(peerId, [
    { click: '#snapshots-create-button' },
    { waitFor: [SNAPSHOT_ENTRY, BUDGET] },
  ]);
  const exported = await dom(peerId, [{ download: ['path', '#snapshots-export-button'] }]);
  const file = String(exported.path ?? '');
  if (!file) throw new Error('S16: the export produced no downloadable file');
  const size = statSync(file).size;
  if (size === 0) throw new Error(`S16: the exported backup is empty (${file})`);
  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'log',
    label: 'backup exported',
    line: `${file.split('/').pop()} — ${size} bytes (no TOTP tokens yet)`,
  });

  // ── The change the backup knows nothing about ───────────────────────────
  await openTotp();
  await dom(peerId, [{ fill: ['#totp-uri-input', TOTP_URI] }]);
  const added = await totpState();
  if (added.codes !== 1 || added.empty !== 0) {
    throw new Error(
      `S16: adding a token after the export did not take (tokens=${added.codes}, empty-state=${added.empty})`,
    );
  }
  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'step',
    label: 'token added after the export',
    line: '1 TOTP entry, absent from the exported file',
  });

  // ── Merge: current items survive ────────────────────────────────────────
  await importAndApply(file, 'merge');
  await openTotp();
  const merged = await totpState();
  if (merged.codes !== 1) {
    throw new Error(
      `S16 FINDING: Merge dropped a local token the backup did not contain ` +
        `(#totp-code-0 x${merged.codes}) — Merge must keep current values`,
    );
  }
  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'log',
    label: 'merge kept local state',
    line: 'the post-export token survived a Merge restore of the older file',
  });

  // ── Replace: the same file wipes it ─────────────────────────────────────
  await importAndApply(file, 'replace');
  await openTotp();
  // Waited for, not read once: the destructive branch has to be given the same
  // chance to land as any other outcome, or a restore that is still applying
  // reads exactly like a restore that did nothing. The wait lapses loudly
  // (`BUDGET`, then the screen's real state in the message).
  const replaced = await waitFor(
    async () => {
      const out = await dom(peerId, [
        { readCount: ['codes', '#totp-code-0'] },
        { readCount: ['empty', '#totp-empty-state'] },
      ]);
      const codes = Number(out['codes'] ?? 0);
      const empty = Number(out['empty'] ?? 0);
      return codes === 0 && empty === 1 ? { codes, empty } : undefined;
    },
    { label: 'the TOTP screen to show the replaced (empty) state', timeoutMs: BUDGET },
  ).catch(async (err: unknown) => {
    const now = await totpState();
    throw new Error(
      `S16 FINDING: Replace did not overwrite local state with the backup — the token added after ` +
        `the export is still listed after ${BUDGET}ms (tokens=${now.codes}, empty-state=${now.empty}). ` +
        `Either the restore applied nothing or it merged where it promised to replace.`,
      { cause: err },
    );
  });

  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'log',
    label: 'replace overwrote local state',
    line:
      `the post-export token is gone; the empty backup state is what the app now holds ` +
      `(tokens=${replaced.codes}, empty-state=${replaced.empty})`,
  });
  const shot = await surface.screenshot(peerId, 'restore-replaced');
  if (shot !== null) ctx.note(`screenshot: ${ctx.rel(shot)}`);
  ctx.note(`S16 OK on ${surface.name}: Merge kept the newer local token, Replace discarded it`);
}
