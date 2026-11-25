// oxlint-disable max-lines -- one file per harness entry point: the surface table, the scenario registry hand-off, the CLI and the evidence/report writer are one program split only by headings
// Tandem e2e harness entry point.
//
//   node test/run.ts --list
//   node test/run.ts --scenario smoke --surfaces web
//   node test/run.ts --scenario S1 --surfaces macos,ios-sim
//   node test/run.ts --surfaces android          (default scenario: smoke)
//   node test/run.ts --evidence-dir <path> ...
//
// Every run writes test/runs/<ts>/ (or --evidence-dir) with per-peer logs,
// PNGs and result.json; on timeout it screenshots + dumps the log tail, never
// hangs. S6 and S9 case 3 are deliberately absent (DECLINED below): they need
// seams that do not exist yet (LAN-denial toggles, a launch-URL source).
import { writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join } from 'node:path';
import { createRun, skippedCapabilities, type RunContext } from './lib/run.ts';
import registry, { type ScenarioEntry } from './scenarios/registry.ts';
import { SeamMissing } from './lib/wait.ts';
import type { Dict, Surface } from './lib/types.ts';

// -- surfaces -----------------------------------------------------------------

const SURFACE_ORDER = ['web', 'macos', 'ios-sim', 'ios-device', 'android'] as const;

interface SurfaceEntry {
  name: string;
  description: string;
  create: (
    runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
    opts: { peers: string[]; timeoutMs: number; seams?: Dict },
  ) => Promise<Surface>;
}

async function surfaceEntries(): Promise<SurfaceEntry[]> {
  const [web, macos, iosSim, iosDevice, android] = await Promise.all([
    import('./surfaces/web.ts'),
    import('./surfaces/macos.ts'),
    import('./surfaces/ios-sim.ts'),
    import('./surfaces/ios-device.ts'),
    import('./surfaces/android.ts'),
  ]);
  return [
    { name: 'web', description: 'Playwright chromium; two isolated contexts = two peers (UI-flow coverage)', create: web.create },
    { name: 'macos', description: 'Built macOS app; PO_INSTANCE per peer, open po:// links, screencapture', create: macos.create },
    { name: 'ios-sim', description: 'iOS simulator via simctl (iPhone 18 Pro); SIMCTL_CHILD_* env', create: iosSim.create },
    { name: 'ios-device', description: 'Physical iPhone via devicectl; xtool signing, DEVICECTL_CHILD_* env', create: iosDevice.create },
    { name: 'android', description: 'Android emulator via adb (Medium_Phone AVD); seams ride the launch intent', create: android.create },
  ];
}

// -- scenarios ----------------------------------------------------------------

// Loaded dynamically, like the surfaces above: the registry is the CLI's module
// table, and a scenario that fails to load should name itself in the error
// rather than break the import graph of every other one. Its `ScenarioEntry` is
// this file's type too — one declaration, so a field like `nativeOnly` cannot
// be added to one and forgotten in the other.
const SCENARIOS: ScenarioEntry[] = registry;

const DECLINED = [
  'S6 — the adb revoke toggle exists, but the ACCESS_LOCAL_NETWORK answer is dropped (MainActivity has no onRequestPermissionsResult, and the Rust check runs before the tracing subscriber exists), so there is no marker to assert on; S18 covers the LAN observability that IS reachable (`event=net_locality`)',
  'S9 case 3 — a pairing link on a signed-in device: web has no launch-URL source, and on native the refusal is a toast with no marker',
  'Biometric gate/toggle — the OS owns the prompt; neither the browser nor simctl can script an accept/deny result',
  'Camera QR *scanning* — needs a live camera feed; the QR scenario covers the render+decode round trip only',
  'Share sheet — a platform UI outside the DOM, with no observable on any surface',
];

// -- CLI ----------------------------------------------------------------------

function parseArgs(argv: string[]): Record<string, string | boolean> {
  const out: Record<string, string | boolean> = {};
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--list') out['list'] = true;
    else if (arg === '--names') out['names'] = true;
    else if (arg === '--scenario') out['scenario'] = argv[++i];
    else if (arg === '--surfaces') out['surfaces'] = argv[++i];
    else if (arg === '--evidence-dir') out['evidence-dir'] = argv[++i];
    else throw new Error(`unknown argument: ${arg}`);
  }
  return out;
}

interface StepResult {
  step: string;
  ok: boolean;
  detail: string;
  /**
   * Capabilities the step recorded as skipped (`support.ts::skip`). A step
   * with skips ran, but those steps asserted NOTHING — a green `ok` is not
   * coverage, and a scenario whose steps all skipped is green on paper only.
   */
  skipped: string[];
}

/** The step a successful run records, capability skips and all. */
function stepCompleted(ctx: RunContext, scenarioName: string): StepResult {
  const skipped = skippedCapabilities(ctx);
  return {
    step: scenarioName,
    ok: true,
    detail:
      skipped.length === 0
        ? 'completed'
        : `completed with ${skipped.length} skipped step(s): ${skipped.join(', ')}`,
    skipped,
  };
}

function isSurfaceName(name: string): name is (typeof SURFACE_ORDER)[number] {
  return SURFACE_ORDER.some((known) => known === name);
}

function resolveSurfaceNames(args: Record<string, string | boolean>): string[] {
  const names = String(args['surfaces'] ?? 'web')
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean);
  for (const name of names) {
    if (!isSurfaceName(name)) {
      throw new Error(`unknown surface '${name}' (have: ${SURFACE_ORDER.join(', ')})`);
    }
  }
  return names;
}

function resolveScenario(name: string): ScenarioEntry {
  const scenario = SCENARIOS.find((s) => s.name === name);
  if (!scenario) throw new Error(`unknown scenario '${name}' (have: ${SCENARIOS.map((s) => s.name).join(', ')})`);
  return scenario;
}

function listCatalog(entries: SurfaceEntry[], args: Record<string, string | boolean>): void {
  // `--names` is the machine-readable form a workflow reads to build a
  // scenario matrix: one name per line, no prose to parse.
  if (args['names'] === true) {
    const names = String(args['surfaces'] ?? '')
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean);
    // A `nativeOnly` scenario asserts something a browser cannot observe; a
    // web-only request must not be handed a name it can only skip.
    const webOnly = names.length > 0 && names.every((name) => name === 'web');
    for (const s of SCENARIOS) {
      if (s.nativeOnly === true && webOnly) continue;
      console.log(s.name);
    }
    return;
  }
  console.log('Surfaces:');
  for (const s of entries) console.log(`  ${s.name.padEnd(9)} ${s.description}`);
  console.log('\nScenarios:');
  for (const s of SCENARIOS) console.log(`  ${s.name.padEnd(5)} ${s.description}`);
  console.log('\nNot automated (missing seams):');
  for (const line of DECLINED) console.log(`  ${line}`);
}

// Reconcile peer letters with each surface's expectations: a surface listed
// once in a 2-peer scenario hosts both peers (the web surface's two browser
// contexts are its two peers); a surface repeated in the list takes one peer
// per occurrence. A scenario that needs MORE peers than surfaces takes the
// extras on its FIRST surface — the DOM-capable one a scenario that needs two
// accounts plus a device is built on (`web` can drive any number of isolated
// contexts; a device cannot).
function planOwnership(surfaceNames: string[], peers: string[]): string[][] {
  // One entry per OCCURRENCE in `surfaceNames`, not per name: `web,web` is two
  // surface instances, one peer each, and keying by name handed both of them
  // both letters — two instances driving the same peer, overwriting its log
  // and screenshots.
  const extra = peers.length - surfaceNames.length;
  return surfaceNames.map((_, i) => (i === 0 ? peers.slice(0, extra + 1) : [peers[extra + i]]));
}

/** One peer letter per surface, plus the extras a scenario asked for. */
function planPeers(surfaceNames: string[], minPeers: number): string[] {
  return Array.from({ length: Math.max(surfaceNames.length, minPeers) }, (_, i) =>
    String.fromCharCode(97 + i),
  );
}

async function setupSurfaces(
  entries: SurfaceEntry[],
  surfaceNames: string[],
  ownership: string[][],
  scenario: ScenarioEntry,
  ctx: RunContext,
  surfaces: Surface[],
): Promise<void> {
  for (const [i, name] of surfaceNames.entries()) {
    const entry = entries.find((e) => e.name === name);
    if (!entry) throw new Error(`surface '${name}' not loadable`);
    const hostPeers = ownership[i] ?? [];
    ctx.note(`setting up surface ${name} (peers: ${hostPeers.join(',')})`);
    // oxlint-disable-next-line no-await-in-loop -- surfaces come up one at a time; concurrent runs on one tree are unsafe (test/README.md)
    const surface = await entry.create(ctx, { peers: hostPeers, timeoutMs: 120_000, seams: scenario.seams });
    // oxlint-disable-next-line no-await-in-loop -- a surface's setup must finish before the next begins
    await surface.setup();
    for (const peer of hostPeers) {
      // Native surfaces take seams through the launch environment (or the
      // equivalent channel: `SIMCTL_CHILD_*`, intent extras); the web surface
      // got them in its init script and ignores this.
      // oxlint-disable-next-line no-await-in-loop -- peers share one device/context, so launches are serial
      await surface.launch(peer, scenario.seams ?? {});
    }
    surfaces.push(surface);
  }
}

// Include the throwing frame: a bare message ("Cannot read properties of
// undefined (reading 'peer')") names no location, and the harness has many
// call sites that could produce it.
function describeFailure(err: unknown): string {
  const where =
    err instanceof Error && err.stack !== undefined && err.stack !== ''
      ? `\n  at ${(err.stack.split('\n').find((l) => l.includes('/test/')) ?? '').trim()}`
      : '';
  return err instanceof SeamMissing
    ? `SEAM MISSING: ${err.message}`
    : String(err instanceof Error ? err.message : err) + where;
}

async function collectEvidence(
  ctx: RunContext,
  surfaces: Surface[],
  surfaceNames: string[],
  peers: string[],
  started: number,
  exitCode: number,
  steps: StepResult[],
  scenarioName: string,
): Promise<void> {
  // Evidence + teardown, always.
  for (const surface of surfaces) {
    for (const peer of surface.peerIds) {
      const text = surface.logs(peer);
      if (text) {
        const dir = ctx.surfaceDir(surface.name);
        writeFileSync(join(dir, `${peer}.log`), ctx.mask(text));
      }
      // oxlint-disable-next-line no-await-in-loop -- peers share one device/context, so they are stopped serially
      await surface.screenshot(peer, 'end-of-run').catch(() => {});
      // oxlint-disable-next-line no-await-in-loop -- peers share one device/context, so they are stopped serially
      await surface.stop(peer).catch(() => {});
    }
    // oxlint-disable-next-line no-await-in-loop -- one surface's teardown must finish before the next
    await surface.teardown().catch(() => {});
  }
  const skipped = skippedCapabilities(ctx);
  const result = {
    scenario: scenarioName,
    surfaces: surfaceNames,
    peers,
    build: surfaces.map((s) => ({ surface: s.name, ...s.build })),
    git_rev: gitRev(),
    started: new Date(started).toISOString(),
    duration_ms: Date.now() - started,
    ok: exitCode === 0,
    // `ok` alone cannot tell a scenario that proved its assertions from one
    // that skipped them; `skipped` is what a release check reads for coverage.
    skipped,
    steps,
    timeline: ctx.timeline,
    evidence_dir: ctx.rel(ctx.dir),
  };
  writeFileSync(join(ctx.dir, 'result.json'), JSON.stringify(result, null, 2));
  const coverage =
    skipped.length === 0
      ? ''
      : ` — NOT coverage: ${skipped.length} skipped (${skipped.join(', ')}), those steps asserted nothing`;
  ctx.note(`result: ${exitCode === 0 ? 'PASS' : 'FAIL'}${coverage} — ${ctx.rel(join(ctx.dir, 'result.json'))}`);
}

async function main(): Promise<number> {
  const args = parseArgs(process.argv.slice(2));
  const entries = await surfaceEntries();

  if (args['list'] === true) {
    listCatalog(entries, args);
    return 0;
  }

  const surfaceNames = resolveSurfaceNames(args);
  const scenarioName = String(args['scenario'] ?? 'smoke');
  const scenario = resolveScenario(scenarioName);
  const peers = planPeers(surfaceNames, scenario.minPeers);
  const evidenceRoot = String(args['evidence-dir'] ?? join('test', 'runs'));
  const ctx = createRun(evidenceRoot, `${scenarioName}-${surfaceNames.join('+')}`);
  ctx.note(`scenario=${scenario.name} surfaces=${surfaceNames.join(',')} peers=${peers.join(',')}`);

  const ownership = planOwnership(surfaceNames, peers);
  const started = Date.now();
  const surfaces: Surface[] = [];
  const steps: StepResult[] = [];
  let exitCode = 0;

  try {
    await setupSurfaces(entries, surfaceNames, ownership, scenario, ctx, surfaces);
    // Single-surface scenarios (smoke, and macos,macos-style repeats) pass
    // letters through surface.peerIds; multi-surface scenarios receive one
    // surface per element with peerIds filtered to that element's letters.
    await scenario.run(ctx, surfaces);
    steps.push(stepCompleted(ctx, scenario.name));
  } catch (err) {
    exitCode = 1;
    const detail = describeFailure(err);
    steps.push({ step: scenario.name, ok: false, detail, skipped: skippedCapabilities(ctx) });
    ctx.note(`FAILED: ${detail}`);
  } finally {
    await collectEvidence(ctx, surfaces, surfaceNames, peers, started, exitCode, steps, scenario.name);
  }
  return exitCode;
}

function gitRev(): string {
  try {
    return execFileSync('git', ['rev-parse', 'HEAD'], { cwd: process.cwd() }).toString().trim();
  } catch {
    return 'unknown';
  }
}

// Force the exit rather than setting `exitCode`: a surface that failed to
// start can leave a handle alive (a half-launched browser, a serving process)
// and Node then keeps the event loop open after the result is already written.
// A run holds the serialization lock while it lingers, so one failed launch
// stalls every later run until something kills it — a 20-minute hang in place
// of an instant, correctly-reported failure. Every write above is synchronous,
// so nothing is truncated here.
process.exit(await main());
