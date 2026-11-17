// smoke — launch the native po app, assert the window came up, screenshot.
//
// po is a native GPUI app, not a web page, so the first-screen assertion is a
// process/window check plus a screenshot as supporting evidence.
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { recordObservation } from './support.ts';

export const name = 'smoke';
export const description = 'Launch po, assert the window is up, screenshot each peer';

/** Surfaces that can capture a screenshot. */
function canScreenshot(surface: Surface): surface is Surface & { screenshot: (peer: string, name: string) => Promise<string> } {
  return 'screenshot' in surface && typeof (surface as Record<string, unknown>).screenshot === 'function';
}

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  for (const surface of surfaces) {
    for (const peer of surface.peerIds) {
      // oxlint-disable-next-line no-await-in-loop -- peers of one surface share its device, so they come up one at a time
      const evidence = await surface.ready(peer);
      recordObservation(ctx, { surface, peer }, 'po window up', evidence);
      if (canScreenshot(surface)) {
        await surface.screenshot(peer, 'smoke');
      }
    }
  }
}
