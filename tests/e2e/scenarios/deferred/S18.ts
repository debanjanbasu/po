// S18 — LAN locality: what this app can honestly know about the local network.
//
// The premise it verifies is documented in docs/device-testing.md § "Local
// network": veilid exposes NO per-peer routing domain, transport or route kind.
// What reaches the app is
//   - `VeilidStateAttachment.local_network_ready` — the LocalNetwork routing
//     domain's readiness, computed from this node's OWN local addresses, so it
//     reads true on a lone machine with no LAN peer anywhere;
//   - `VeilidStateNetwork.peers[].peer_address` — the observed remote address
//     of each recently-accessed peer, as a display string, from which locality
//     is inferred (`src/veilid/core/updates.rs::locality_seam`).
// The seam emits both as one `event=net_locality` line whenever either changes.
//
// So this scenario asserts the strongest honest thing per peer:
//   1. the marker exists and its fields parse;
//   2. `local_network_ready=true` — the node DID reach its local-network
//      routing domain. A denied local-network privilege on iOS/macOS keeps the
//      domain from ever becoming usable, and Apple exposes no API to read the
//      privilege, so this is the closest observable to that permission outcome;
//   3. `local_peers` — peers whose observed address is a LAN address. Zero is
//      recorded as an OBSERVATION, never a failure: a peer table holding only
//      internet bootstrap nodes is normal for a node that has not met a LAN
//      peer yet, and a zero does not disprove that the LAN path works.

import { skip, waitMarkerField } from './support.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';

export const name = 'S18';
export const description =
  'LAN locality: assert the local-network routing domain and classify observed peers by address (native only)';

const MARKER = 'net_locality';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  if (surfaces.length === 0) throw new Error('S18 needs at least one surface');
  // A web peer is not evidence: WASM Veilid has no UDP and shares the host's
  // network stack, so neither `local_network_ready` nor a peer address says
  // anything about this LAN (docs/device-testing.md § Local network).
  const native = surfaces.filter((surface) => surface.name !== 'web');
  const skipped = surfaces.length - native.length;
  if (skipped > 0) {
    skip(
      ctx,
      { surface: surfaces[0], peer: surfaces[0].peerIds[0] ?? 'a' },
      'LAN verdict',
      `${skipped} web peer(s) excluded — WASM Veilid has no UDP, so its locality fields are meaningless`,
    );
  }
  if (native.length === 0) return;
  // One peer per surface instance: a repeated `--surfaces macos,macos` hosts
  // one process per instance, so a `for (const peer of surface.peerIds)` loop
  // here would read the same two peers twice and skip neither instance's own.
  for (const surface of native) {
    const peer = surface.peerIds[0];
    if (!peer) continue;
    // oxlint-disable-next-line no-await-in-loop -- surfaces come up one at a time; concurrent runs on one tree are unsafe (test/README.md)
    const hit = await waitMarkerField(
      ctx,
      { surface, peer },
      MARKER,
      'local_network_ready',
      'true',
      'local-network routing domain ready',
      120_000,
    );
    ctx.record({
      surface: surface.name,
      peer,
      kind: 'step',
      label: 'observed peer locality',
      line:
        `${hit.fields['peers'] ?? '?'} recently-accessed peer(s), ` +
        `${hit.fields['local_peers'] ?? '?'} on a LAN address — ` +
        'a zero is an observation, not a failure (no LAN peer met yet)',
    });
  }
}

