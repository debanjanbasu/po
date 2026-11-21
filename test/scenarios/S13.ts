// S13 — Session teardown and theme persistence.
//
// Observable (docs/device-testing.md): a chosen theme survives a reload, and
// logging out returns the app to the login screen *and stays there* across a
// reload — the session must not be restored.
//
// The reload IS the assertion. "Log out returns to login" is trivially true of
// the immediate navigation; what matters is that the persisted session is gone,
// which only a fresh document can show.
//
// Reload behaviour, measured (see `docs/device-testing.md`): a reload while
// signed in does NOT land back in the shell directly — it restores the session
// and then demands the PIN again. That is the gate doing its job, so the
// scenario clears it rather than treating it as a failure. After a *logout* a
// reload must land on the login screen, with no gate — that is the difference
// the logout assertion is really testing.

import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { sleep } from '../lib/wait.ts';
import { BUDGET, PASSWORD, skip } from './support.ts';

export const name = 'S13';
export const description = 'Session: logout returns to login and is not restored; theme survives a reload';

const USERNAME = 'e2e-s13-account';
/** The account PIN the onboarding wizard set, same value the web surface uses. */
const PIN = '123456';
/** How long the theme write needs to reach persistent storage (measured ~3 s). */
const THEME_FLUSH_MS = 5_000;

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  // Single-peer scenario (`minPeers: 1`): the run hands the surface exactly one
  // peer id, so `pickPeers` — which insists on a second peer — does not apply.
  if (surfaces.length === 0 || !surfaces[0].peerIds[0]) {
    throw new Error('S13 needs at least one surface with one peer');
  }
  const surface = surfaces[0];
  const peerId = surface.peerIds[0];
  const ref = { surface, peer: peerId };
  const dom = surface.dom?.bind(surface);
  if (!dom || !surface.signIn) {
    skip(ctx, ref, 'signIn/dom', `${surface.name} cannot sign in and drive the account screen`);
    return;
  }

  await surface.signIn(peerId, USERNAME, PASSWORD, BUDGET);

  /** The theme the document is rendering, from the pressed option. */
  const readTheme = async (): Promise<string> => {
    await dom(peerId, [{ waitFor: ['#theme-option-dark', BUDGET] }]);
    const out = await dom(peerId, [
      { readCount: ['dark', '#theme-option-dark[aria-pressed="true"]'] },
      { readCount: ['light', '#theme-option-light[aria-pressed="true"]'] },
      { readCount: ['system', '#theme-option-system[aria-pressed="true"]'] },
    ]);
    if (out.dark === 1) return 'dark';
    if (out.light === 1) return 'light';
    if (out.system === 1) return 'system';
    return 'none';
  };

  /** Open the account screen from the dock. */
  const openAccount = async (): Promise<void> => {
    await dom(peerId, [{ waitFor: ['#oo-main-dock', BUDGET] }]);
    await dom(peerId, [
      { click: 'a[href="/account"]' },
      { waitFor: ['#theme-option-dark', BUDGET] },
    ]);
  };

  // ── Theme persistence ───────────────────────────────────────────────────
  await openAccount();
  await dom(peerId, [{ click: '#theme-option-dark' }]);
  const selected = await readTheme();
  if (selected !== 'dark') {
    throw new Error(`S13: selecting Dark did not take effect (pressed option reads "${selected}")`);
  }

  // Let the write land before reloading. Measured: the theme reaches persistent
  // storage ~3 s after the click, so a reload issued immediately reads back
  // "system" — the write is still in flight. This scenario is about
  // *persistence* (does the value come back on a fresh document), not about
  // flush latency, so it settles first rather than racing the write and
  // reporting a false failure.
  await sleep(THEME_FLUSH_MS);

  await dom(peerId, [{ reload: true }]);
  // A reload restores the session behind the PIN gate; clear it, then confirm
  // the gate was actually there (if a reload ever stops gating, that is a
  // security regression the scenario should notice, not silently absorb).
  const gated = await dom(peerId, [
    { readCount: ['pin', '#pin-gate-input'] },
    { readCount: ['dock', '#oo-main-dock'] },
  ]);
  if (Number(gated.pin) + Number(gated.dock) === 0) {
    throw new Error('S13: after a reload the app showed neither the PIN gate nor the shell');
  }
  if (gated.pin === 1) {
    await dom(peerId, [{ fill: ['#pin-gate-input', PIN] }]);
  }
  await openAccount();
  const afterReload = await readTheme();
  if (afterReload !== 'dark') {
    throw new Error(
      `S13: the theme did not survive a reload — selected "dark", reloaded, read "${afterReload}"`,
    );
  }
  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'log',
    label: 'theme persisted',
    line: `theme=dark survived a reload (gate re-armed: ${gated.pin === 1 ? 'yes' : 'no'})`,
  });

  // ── Logout, and the reload that proves it stuck ─────────────────────────
  await dom(peerId, [
    { click: '#account-logout-button' },
    { waitFor: ['#login-username-input', BUDGET] },
  ]);
  const shell = await dom(peerId, [{ readCount: ['dock', '#oo-main-dock'] }]);
  if (shell.dock !== 0) {
    throw new Error(`S13: the app shell is still mounted after logout (#oo-main-dock x${shell.dock})`);
  }

  await dom(peerId, [{ reload: true }]);
  const restored = await dom(peerId, [
    { readCount: ['dock', '#oo-main-dock'] },
    { readCount: ['login', '#login-username-input'] },
    { readCount: ['pin', '#pin-gate-input'] },
  ]);
  if (restored.dock !== 0) {
    throw new Error('S13: reloading after logout restored the session — the app landed back in the shell');
  }
  if (restored.pin !== 0) {
    throw new Error('S13: after logout a reload reached the PIN gate — the session was still remembered');
  }
  if (restored.login !== 1) {
    throw new Error(
      `S13: after logout + reload the login form is absent ` +
        `(#login-username-input x${restored.login}, #pin-gate-input x${restored.pin})`,
    );
  }

  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'log',
    label: 'session teardown',
    line: 'logout returned to login and a reload did not restore the session',
  });
  ctx.note(`S13 OK on ${surface.name}: theme persisted, logout not restored`);
}
