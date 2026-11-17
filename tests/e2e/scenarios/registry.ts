// The scenario registry for po's e2e harness.
//
// v1 ships only the smoke scenario; the peer-to-peer/DHT scenarios migrated
// from oo live in `./deferred/` until po's sync layer exists.
import type { Dict, Surface } from '../lib/types.ts';
import type { RunContext } from '../lib/run.ts';

export interface ScenarioEntry {
  name: string;
  description: string;
  run: (ctx: RunContext, surfaces: Surface[]) => Promise<void>;
  minPeers: number;
  nativeOnly?: true;
  seams?: Dict;
}

const SCENARIOS: ScenarioEntry[] = [
  { ...(await import('./smoke.ts')), minPeers: 1 },
];

export function scenarios(): ScenarioEntry[] {
  return SCENARIOS;
}
