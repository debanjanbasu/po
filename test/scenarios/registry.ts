// The scenario registry, split out of the CLI entry point: the CLI's
// `--list` output and the `ScenarioEntry` table need every module's metadata,
// and listing them here keeps the loading order explicit (anchor→joiner
// dispatch reads the same order).
import type { Dict, Surface } from '../lib/types.ts';
import type { RunContext } from '../lib/run.ts';

export interface ScenarioEntry {
  name: string;
  description: string;
  run: (ctx: RunContext, surfaces: Surface[]) => Promise<void>;
  /** Surfaces this scenario needs, in anchor→joiner order. Default: any. */
  minPeers: number;
  /**
   * A web peer is not evidence for this scenario: it asserts something a
   * browser cannot observe (S18's LAN locality is native-only by construction).
   */
  nativeOnly?: true;
  /** `PO_E2E_*` values this scenario needs at boot; see `SurfaceOptions.seams`. */
  seams?: Dict;
}

// Loaded dynamically, like the surfaces above: the registry is the CLI's module
// table, and a scenario that fails to load should name itself in the error
// rather than break the import graph of every other one.
const SCENARIOS: ScenarioEntry[] = [
  { ...(await import('./smoke.ts')), minPeers: 1 },
  { ...(await import('./S1.ts')), minPeers: 2 },
  { ...(await import('./S2.ts')), minPeers: 2 },
  { ...(await import('./S3.ts')), minPeers: 2 },
  { ...(await import('./S4.ts')), minPeers: 2 },
  { ...(await import('./S5.ts')), minPeers: 2 },
  { ...(await import('./S7.ts')), minPeers: 2 },
  { ...(await import('./S8.ts')), minPeers: 2 },
  { ...(await import('./S9.ts')), minPeers: 2 },
  // Single-peer feature flows driven through the generic `dom` surface op.
  { ...(await import('./S10.ts')), minPeers: 1 },
  { ...(await import('./S11.ts')), minPeers: 1 },
  { ...(await import('./S12.ts')), minPeers: 1 },
  { ...(await import('./S13.ts')), minPeers: 1 },
  // Feature-side coverage added after the audit: the invite decline path, the
  // typing signal, and the destructive half of the restore (Replace).
  { ...(await import('./S14.ts')), minPeers: 2 },
  { ...(await import('./S15.ts')), minPeers: 2 },
  { ...(await import('./S16.ts')), minPeers: 1 },
  { ...(await import('./QR.ts')), minPeers: 2 },
  // Messaging on a surface with no typing channel: the DOM surface hosts the
  // anchor and the counterpart, the joiner pairs from the pairing alone and
  // takes part in the conversation. minPeers 3 gives the DOM surface the extra
  // peer (test/run.ts planOwnership).
  { ...(await import('./S17.ts')), minPeers: 3 },
  // LAN locality on native surfaces: native-only by construction (web has no
  // UDP, so a web peer's LocalNetwork verdict is meaningless).
  { ...(await import('./S18.ts')), minPeers: 1, nativeOnly: true },
];

export default SCENARIOS;
