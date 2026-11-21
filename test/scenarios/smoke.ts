// smoke — launch every surface's peers, assert the first screen, screenshot.
//
// The first screen is asserted in the DOM (`Surface.queryDom`), not from the
// screenshot: Android is adb-only, so until its DevTools channel existed a
// "first screen" assertion meant *a screenshot was taken* — and a permanently
// frozen splash captures exactly like a working screen (the 2026-11-13 render
// wedge passed every smoke run for days while the app was alive but not
// rendering). The screenshot stays as supporting evidence, never the claim.
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { waitFor } from '../lib/wait.ts';
import { BUDGET, lastMarkerLine, recordObservation, skip, type PeerRef } from './support.ts';

export const name = 'smoke';
export const description = 'Launch, assert first screen, screenshot each peer';

/**
 * The login/first-run anchor in the rendered DOM.
 *
 * `#login-username-input` carries `aria-label="Login username"`
 * (src/components/auth/login_signup.rs) and is the element the live Android
 * WebView answered with over CDP (`input[aria-label="Login username"]` →
 * `#login-username-input`), so the scenario pins the accessible name rather
 * than a class or layout detail. Not a const on the surface: the selector is
 * the scenario's claim about which screen is up.
 */
const FIRST_SCREEN_SELECTOR = 'input[aria-label="Login username"]';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  for (const surface of surfaces) {
    for (const peer of surface.peerIds) {
      // oxlint-disable-next-line no-await-in-loop -- peers of one surface share its device/browser, so they come up one at a time
      const evidence = await surface.ready(peer);
      // By name, not via `ready()`'s any-marker accept: the app reached its
      // local-first startup (src/app/startup.rs) — the load-bearing line on
      // native, where `ready()` settles for the first `event=` of anything.
      recordObservation(
        ctx,
        { surface, peer },
        'app started',
        markerLineOrThrow(surface, peer, 'app_started', evidence),
      );
      // The first screen must be IN THE DOM, not merely on the screen:
      // `app_started` and a screenshot both pass on a wedged splash.
      // oxlint-disable-next-line no-await-in-loop -- peers of one surface share its device/browser, so they come up one at a time
      await assertFirstScreenInDom(ctx, surface, peer);
      ctx.note(`${surface.name}/${peer}: app_started + first screen OK — ${evidence}`);
      // oxlint-disable-next-line no-await-in-loop -- one screenshot at a time on that same shared device
      const shot = await surface.screenshot(peer, 'first-screen');
      if (shot !== null) ctx.note(`screenshot: ${ctx.rel(shot)}`);
    }
  }
}

/**
 * The rendered-DOM half of the first-screen assertion.
 *
 * A surface without a DOM channel records a skip — a green run must still say
 * what it did not assert (test/README.md § "A skip is not a pass"). A surface
 * with the channel that cannot find the anchor fails here with the selector
 * named: that is the failure a frozen-but-alive app used to escape.
 *
 * The read is a poll, not a single shot: the login form renders a beat after
 * the `event=app_started` line `ready()` settles on, and the WebView's
 * DevTools socket exists only once that webview does. A probe that cannot
 * reach the socket yet (surface-internal waits own that path) returns null
 * and lets the deadline do the failing — the run log carries the cause.
 */
async function assertFirstScreenInDom(ctx: RunContext, surface: Surface, peer: string): Promise<void> {
  const ref: PeerRef = { surface, peer };
  const queryDom = surface.queryDom?.bind(surface);
  if (queryDom === undefined) {
    skip(
      ctx,
      ref,
      'queryDom',
      'no DOM channel on this surface — the first screen rests on ready() + the screenshot only',
    );
    return;
  }
  const match = await waitFor(
    async () => {
      try {
        return await queryDom(peer, FIRST_SCREEN_SELECTOR);
      } catch (err) {
        ctx.note(`queryDom probe failed (will retry): ${err instanceof Error ? err.message : String(err)}`);
        return null;
      }
    },
    {
      label: `${surface.name}/${peer} first screen rendered (${FIRST_SCREEN_SELECTOR})`,
      timeoutMs: BUDGET,
      intervalMs: 1000,
    },
  );
  recordObservation(ctx, ref, 'first screen rendered', `${FIRST_SCREEN_SELECTOR} → ${match}`);
  ctx.note(`${surface.name}/${peer}: first screen rendered — ${FIRST_SCREEN_SELECTOR} → ${match}`);
}

/** The `event=<marker>` line a launch must have produced by now. */
function markerLineOrThrow(
  surface: Surface,
  peer: string,
  marker: string,
  evidence: string,
): string {
  const line = lastMarkerLine(surface.logs(peer), marker);
  if (line === undefined) {
    throw new Error(
      `${surface.name}/${peer}: no event=${marker} in the log, yet the first screen was reached (${evidence})`,
    );
  }
  return line;
}
