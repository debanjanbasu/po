// S11 — Security: credential change.
//
// Observable (docs/device-testing.md § S11): after a password change the OLD
// password is refused and the NEW one admits, and a wrong PIN is refused at
// the second-factor gate.
//
// The change also re-encrypts the stored keypair under the new password's
// derived DHT secret (`src/user/record/reencrypt.rs`), and the login round
// trip is its observable: `user::auth::check_credentials` decrypts the stored
// keypair with the submitted password, so the same local store must reject the
// old password and accept the new one. There is no `event=` marker on that
// path, so the round trip itself is the evidence.
//
// A password can only be checked from a signed-out login screen, so the flow
// is: change on /account/security → log out → the old password is refused →
// the new one admits and answers the PIN gate.
import { waitFor } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { DomOp, Surface } from '../lib/types.ts';
import { BUDGET, PASSWORD, skip } from './support.ts';

export const name = 'S11';
export const description =
  'Security: password change (old refused, new admits) and a wrong PIN refused at the gate';

/** A dedicated account: a half-applied change cannot strand another
 *  scenario's user, and each run starts from a fresh browser context anyway. */
const USERNAME = 'e2e-s11-credentials';
/** Same shape as `PASSWORD`: the app's rule is 8-256 chars with a digit, a
 *  lower, an upper and a special (`user::validation::validate_password`). */
const NEW_PASSWORD = 'E2e-Passw0rd!y';
/** The account's second factor — the PIN the harness's onboarding sets. */
const PIN = '123456';
/** Six digits, so only the value differs from the account PIN. */
const WRONG_PIN = '654321';
/** The app's own texts, asserted verbatim. */
const LOGIN_REFUSAL = "Credentials don't match.";
const PIN_REFUSAL = 'Incorrect PIN';
/** The gate's inline hint: the `p.validator-hint` after the input's label. */
const GATE_HINT = 'label:has(#pin-gate-input) + p.validator-hint';

/** One element's text, or null when it is not on screen. */
async function readIfPresent(
  dom: (peer: string, ops: DomOp[]) => Promise<Record<string, string | number>>,
  peer: string,
  selector: string,
): Promise<string | null> {
  const counts = await dom(peer, [{ readCount: ['n', selector] }]);
  if (Number(counts['n']) === 0) return null;
  const text = await dom(peer, [{ readText: ['text', selector] }]);
  return String(text['text']);
}

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  if (surfaces.length === 0) throw new Error('S11 needs one surface');
  const surface = surfaces[0];
  const peer = surface.peerIds[0];
  const ref = { surface, peer };
  const dom = surface.dom?.bind(surface);
  if (!dom || !surface.signIn) {
    skip(ctx, ref, 'dom/signIn', `${surface.name} cannot drive the security screen`);
    return;
  }
  const step = (label: string, line: string): void => {
    ctx.record({ surface: surface.name, peer, kind: 'log', label, line });
  };

  // ── Sign in ─────────────────────────────────────────────────────────────
  await surface.signIn(peer, USERNAME, PASSWORD, BUDGET);
  step('signed in', `${USERNAME} admitted with the account password`);

  // ── Change the password on /account/security ────────────────────────────
  // The change form has no submit button: it auto-submits once all three
  // fields are valid.
  await dom(peer, [
    { click: '#oo-main-dock a[href="/account"]' },
    { waitFor: ['a[href="/account/security"]', BUDGET] },
    { click: 'a[href="/account/security"]' },
    { waitFor: ['#security-password-current', BUDGET] },
    { fill: ['#security-password-current', PASSWORD] },
    { fill: ['#security-password-new', NEW_PASSWORD] },
    { fill: ['#security-password-confirm', NEW_PASSWORD] },
  ]);
  const changed = await waitFor(
    async () => {
      const error = await readIfPresent(dom, peer, 'div[role="alert"]');
      if (error !== null) throw new Error(`the password change was refused: ${error}`);
      return readIfPresent(dom, peer, 'div[role="status"]');
    },
    { label: 'password-change feedback', timeoutMs: BUDGET },
  );
  step('password changed', changed);

  // ── Log out: the login screen is where a password can be checked ────────
  await dom(peer, [
    { click: '#oo-main-dock a[href="/account"]' },
    { waitFor: ['button:has-text("Log out")', BUDGET] },
    { click: 'button:has-text("Log out")' },
    { waitFor: ['#login-username-input', BUDGET] },
  ]);

  // ── The OLD password is refused ─────────────────────────────────────────
  await dom(peer, [
    { fill: ['#login-username-input', USERNAME] },
    { fill: ['#login-password-input', PASSWORD] },
  ]);
  const answer = await waitFor(
    async () => {
      const refusal = await readIfPresent(dom, peer, 'div[aria-live="polite"] span.text-error');
      if (refusal !== null) return `refused: ${refusal}`;
      const dock = await dom(peer, [{ readCount: ['n', '#oo-main-dock'] }]);
      return Number(dock['n']) > 0 ? 'admitted' : null;
    },
    { label: 'the login screen to answer the old password', timeoutMs: BUDGET },
  );
  if (answer === 'admitted') {
    throw new Error(
      'the OLD password still admits after the change — the keypair was not re-encrypted',
    );
  }
  if (answer !== `refused: ${LOGIN_REFUSAL}`) {
    throw new Error(`the old password was refused with an unexpected text: ${answer}`);
  }
  step('old password refused', answer);

  // ── The NEW password admits ─────────────────────────────────────────────
  await dom(peer, [{ fill: ['#login-password-input', NEW_PASSWORD] }]);
  const reached = await waitFor(
    async () => {
      const counts = await dom(peer, [
        { readCount: ['pin', '#pin-gate-input'] },
        { readCount: ['dock', '#oo-main-dock'] },
      ]);
      if (Number(counts['pin']) > 0) return 'pin';
      if (Number(counts['dock']) > 0) return 'dock';
      return null;
    },
    { label: 'admission with the new password', timeoutMs: BUDGET },
  );
  if (reached !== 'pin') {
    throw new Error('the new password admitted without the second-factor PIN gate');
  }
  step('new password admits', `admitted ${USERNAME} and reached the PIN gate`);

  // ── A wrong PIN is refused at the gate; the right one admits ────────────
  await dom(peer, [{ fill: ['#pin-gate-input', WRONG_PIN] }]);
  const pinRefusal = await waitFor(
    async () => {
      const hint = await readIfPresent(dom, peer, GATE_HINT);
      return hint?.includes(PIN_REFUSAL) === true ? hint : null;
    },
    { label: 'the gate to refuse the wrong PIN', timeoutMs: BUDGET },
  );
  step('wrong PIN refused', pinRefusal);

  await dom(peer, [
    { fill: ['#pin-gate-input', PIN] },
    { waitFor: ['#oo-main-dock', BUDGET] },
  ]);
  step('correct PIN admits', 'the gate cleared and the app dock rendered');

  ctx.note(
    `S11 OK on ${surface.name}/${peer}: ${USERNAME} changed its password, the old one was ` +
      `refused ("${LOGIN_REFUSAL}"), the new one admitted, and the gate refused a wrong PIN ` +
      `("${PIN_REFUSAL}")`,
  );
}
