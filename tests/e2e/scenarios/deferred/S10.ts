// S10 — TOTP authenticator.
//
// Observable (docs/device-testing.md): a token added on the TOTP screen
// renders a code that matches an INDEPENDENTLY computed RFC 6238 TOTP for the
// same secret and time step, and deleting it removes the entry.
//
// The point is the recomputation: the app's own `totp_rs` output is compared
// against this file's `node:crypto` HMAC-SHA1, so a wrong algorithm, digit
// count, period or time-step origin shows up as a mismatch instead of a
// self-consistent-but-wrong pass.

import { createHmac } from 'node:crypto';
import type { RunContext } from '../lib/run.ts';
import type { DomOp, Surface } from '../lib/types.ts';
import { BUDGET, PASSWORD, skip } from './support.ts';

export const name = 'S10';
export const description =
  'TOTP: an added secret renders a code that matches an independently computed RFC 6238 one';

/**
 * The RFC 6238 Appendix B seed: ASCII "12345678901234567890" (20 bytes).
 *
 * NOT the RFC 4226 80-bit `JBSWY3DPEHPK3PXP`: the app parses the secret
 * through `totp_rs`, which refuses secrets under 128 bits
 * (`TotpError::SecretTooShort`), so the 80-bit example never reaches a token.
 */
const SECRET = 'GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ';
const PERIOD_SECS = 30;
const DIGITS = 6;
/** `otpauth://` URI is the only input the screen has (the QR scanner fills it too). */
const URI = `otpauth://totp/E2E:s10?secret=${SECRET}&issuer=E2E&algorithm=SHA1&digits=${DIGITS}&period=${PERIOD_SECS}`;
const USERNAME = 'e2e-s10-account';

/** RFC 4648 base32 (no padding) — the encoding every TOTP app stores. */
function base32Decode(secret: string): Buffer {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits = '';
  for (const ch of secret.replace(/=+$/u, '').toUpperCase()) {
    const value = alphabet.indexOf(ch);
    if (value < 0) throw new Error(`S10: invalid base32 character '${ch}' in the test secret`);
    bits += value.toString(2).padStart(5, '0');
  }
  const bytes: number[] = [];
  for (let i = 0; i + 8 <= bits.length; i += 8) bytes.push(Number.parseInt(bits.slice(i, i + 8), 2));
  return Buffer.from(bytes);
}

/** RFC 6238 HOTP/TOTP: HMAC-SHA1 over the big-endian counter, dynamic truncation. */
function totpAt(step: number): string {
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(step));
  const mac = createHmac('sha1', base32Decode(SECRET)).update(counter).digest();
  const offset = mac[mac.length - 1] & 0x0f;
  const truncated =
    ((mac[offset] & 0x7f) << 24) | (mac[offset + 1] << 16) | (mac[offset + 2] << 8) | mac[offset + 3];
  return String(truncated % 10 ** DIGITS).padStart(DIGITS, '0');
}

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  // Single-peer scenario (`minPeers: 1`): the run hands the surface exactly one
  // peer id, so `pickPeers` — which insists on a second peer — does not apply.
  if (surfaces.length === 0 || !surfaces[0].peerIds[0]) {
    throw new Error('S10 needs at least one surface with one peer');
  }
  const surface = surfaces[0];
  const peerId = surface.peerIds[0];
  const ref = { surface, peer: peerId };
  const dom = surface.dom?.bind(surface);
  const signIn = surface.signIn?.bind(surface);
  if (!dom || !signIn) {
    skip(ctx, ref, 'signIn/dom', `${surface.name} cannot sign in and drive the TOTP screen`);
    return;
  }

  await signIn(peerId, USERNAME, PASSWORD, BUDGET);
  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'step',
    label: 'signed in',
    line: `${USERNAME} reached the app shell`,
  });

  // Add the known secret and read the code the app rendered for it.
  const addToken: DomOp[] = [
    { click: '#po-main-dock a[href="/totp"]' },
    { waitFor: ['#totp-uri-input', BUDGET] },
    { fill: ['#totp-uri-input', URI] },
    { waitFor: ['#totp-code-0', BUDGET] },
    { readText: ['code', '#totp-code-0'] },
  ];
  const added = await dom(peerId, addToken);
  const rendered = String(added['code'] ?? '').trim();
  const step = Math.floor(Date.now() / 1000 / PERIOD_SECS);

  // Accept the adjacent step: the second can roll between the app's render and
  // this check (and both codes live in the same wall-clock second at a
  // boundary), so step-1/step/step+1 all pass.
  const expected = new Map([[step - 1, totpAt(step - 1)], [step, totpAt(step)], [step + 1, totpAt(step + 1)]]);
  const matchedStep = [...expected.entries()].find(([, code]) => code === rendered)?.[0];

  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'step',
    label: 'Rendered TOTP code',
    line: `secret=${SECRET} now=${step} rendered=${rendered} expected[${step - 1},${step},${step + 1}]=${[...expected.values()].join(',')}`,
  });
  const shot = await surface.screenshot(peerId, 'totp-code');
  if (shot !== null) ctx.note(`screenshot: ${ctx.rel(shot)}`);

  if (!/^\d+$/u.test(rendered)) {
    throw new Error(
      `S10 FINDING: the app rendered ${JSON.stringify(rendered)} for secret ${SECRET} (step ${step}), which is not a numeric code`,
    );
  }
  if (matchedStep === undefined) {
    throw new Error(
      'S10 FINDING: the app\'s TOTP does not match a standard RFC 6238 code — ' +
        `secret=${SECRET} step=${step} (adjacent ${step - 1}/+1 also allowed) ` +
        `rendered=${rendered} expected=${expected.get(step)} ` +
        `(neighbours ${expected.get(step - 1)},${expected.get(step + 1)}) digits=${rendered.length}/${DIGITS} period=${PERIOD_SECS}s hmac=sha1`,
    );
  }
  ctx.note(
    `S10: app code ${rendered} matches independently computed RFC 6238 code for step ${matchedStep} (now=${step})`,
  );

  // Remove the token and assert the entry (and its empty state) is gone.
  const removeToken: DomOp[] = [
    { click: '#totp-remove-0' },
    { waitFor: ['#totp-empty-state', BUDGET] },
    { readCount: ['codes', '#totp-code-0'] },
  ];
  const removed = await dom(peerId, removeToken);
  ctx.record({
    surface: surface.name,
    peer: peerId,
    kind: 'step',
    label: 'token removed',
    line: `#totp-code-0 count after removal = ${removed['codes']}`,
  });
  if (Number(removed['codes']) !== 0) {
    throw new Error(`S10: the token was not removed — ${removed['codes']} code element(s) still rendered`);
  }

  ctx.note(
    `S10 OK on ${surface.name}/${peerId}: secret ${SECRET} rendered ${rendered}, matching the independently ` +
      'computed RFC 6238 code, and the entry was deleted',
  );
}
