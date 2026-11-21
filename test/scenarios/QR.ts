// QR — the anchor generates a share; the rendered QR must decode back to the
// exact share URL that was handed to the joiner.
//
// Observables (docs/device-testing.md § QR): `event=qr link=<url>` on the
// anchor, and the link equals the share URL the joiner receives. The app
// decodes its own QR with its own scanner (`src/platform/qr.rs`); a lossy
// WebP or a mismatched URL surfaces as `event=qr_decode_failed`.
import { findMarker, waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { BUDGET, PASSWORD, pairingShareUrl, pickPeers, recordObservation, skip } from './support.ts';

export const name = 'QR';
export const description =
  'QR round-trip: the rendered QR decodes back to the exact share URL handed to the joiner';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  const { first: anchor, second: joiner } = pickPeers(surfaces);
  const signIn = anchor.surface.signIn?.bind(anchor.surface);
  const openShareScreen = anchor.surface.openShareScreen?.bind(anchor.surface);
  const readShare = anchor.surface.readShare?.bind(anchor.surface);
  if (!signIn || !openShareScreen || !readShare) {
    skip(
      ctx,
      anchor,
      'signIn/openShareScreen/readShare',
      `${anchor.surface.name} cannot sign in and open the share screen ` +
        `(signIn=${Boolean(signIn)} openShareScreen=${Boolean(openShareScreen)} readShare=${Boolean(readShare)})`,
    );
    return;
  }

  // The anchor's own account. The joiner is only used to show that the URL
  // the anchor hands out is the one that was encoded.
  await signIn(anchor.peer, 'e2e-qr-anchor', PASSWORD, BUDGET);
  await openShareScreen(anchor.peer, PASSWORD, BUDGET);

  // The share the joiner would receive: the deep link the app produced, and
  // the https share URL that is what the QR actually encodes.
  const share = await readShare(anchor.peer, BUDGET);
  const expectedLink = pairingShareUrl(share.deepLink);
  ctx.record({
    surface: anchor.surface.name,
    peer: anchor.peer,
    kind: 'log',
    label: 'share handed to the joiner',
    line: `deep_link=${ctx.mask(share.deepLink)} share_url=${ctx.mask(expectedLink)}`,
  });

  // Marker-first: `event=qr link=<url>` is emitted by the app's own decode of
  // the QR it just rendered.
  const qr = await waitForSignal(
    ctx,
    anchor.surface,
    anchor.peer,
    { label: 'QR round-trip', marker: 'qr' },
    BUDGET,
  );
  const decoded = qr.fields['link'];
  if (!decoded) {
    throw new Error(`event=qr line has no link field: ${ctx.mask(qr.line)}`);
  }
  if (decoded !== expectedLink) {
    throw new Error(
      `the QR decoded to a different URL than the share handed to the joiner\n` +
        `  qr link:    ${ctx.mask(decoded)}\n` +
        `  share url:  ${ctx.mask(expectedLink)}`,
    );
  }

  // A decode failure on the same share is a hard failure, not a soft one.
  const failed = findMarker(anchor.surface.logs(anchor.peer), 'qr_decode_failed');
  if (failed !== undefined) {
    throw new Error(`the app could not read its own QR back: ${ctx.mask(failed)}`);
  }
  recordObservation(
    ctx,
    anchor,
    'QR decoded without failure',
    `event=qr_decode_failed absent, link=${ctx.mask(decoded)}`,
  );

  // The joiner must be able to consume the very same URL: the deep link the
  // anchor produced carries the same payload as the QR's share URL.
  if (!share.deepLink.includes('data=')) {
    throw new Error(`the share deep link carries no payload: ${ctx.mask(share.deepLink)}`);
  }
  // `caps.deepLink`, not the mere presence of `openUrl`: every surface has the
  // method, but a browser has no launch-URL source for a pairing link, so this
  // step genuinely cannot run there.
  if (joiner.surface.caps.deepLink) {
    await joiner.surface.openUrl(joiner.peer, share.deepLink);
    ctx.record({
      surface: joiner.surface.name,
      peer: joiner.peer,
      kind: 'log',
      label: 'share URL delivered to the joiner',
      line: `same payload as the QR: ${ctx.mask(expectedLink)}`,
    });
  } else {
    skip(
      ctx,
      joiner,
      'openUrl',
      `${joiner.surface.name} cannot deliver a deep link to the joiner`,
    );
  }

  ctx.note(
    `QR OK on ${anchor.surface.name}/${anchor.peer}: the QR decoded back to the share URL handed to the joiner`,
  );
}
