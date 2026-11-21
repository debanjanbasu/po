// iOS simulator surface — `xcrun simctl` only: boot/erase/install/launch
// --console/openurl/io screenshot/privacy grant, SIMCTL_CHILD_* env.
import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { definedEnv, exec, execBuffer, spawnLogged, type Captured } from '../lib/proc.ts';
import { infoPlist } from './ios-sim-info-plist.ts';
import { isStale, E2E_SEAM_FEATURE } from '../lib/build.ts';
import { surfaceShotPath } from '../lib/run.ts';
import { readIdentityMarker, readShareMarker, waitFor } from '../lib/wait.ts';
import type { Dict, SurfaceOptions } from '../lib/types.ts';

const CARGO = 'cargo';
const IOS_SIM_TARGET = 'aarch64-apple-ios-sim';
const BUNDLE_DIR = join(process.cwd(), 'target', 'e2e-ios-sim', 'Oo.app');
const BUNDLE_BIN = join(BUNDLE_DIR, 'Oo');
const BUNDLE_ID = 'com.debanjanbasu.po';
/** Preferred simulator name; resolved against `simctl list` at setup time. */
const DEVICE_PREFERENCE = 'iPhone 18 Pro';
/** The resolved device name (set by `resolveDeviceName`). */
let DEVICE = DEVICE_PREFERENCE;

/**
 * Resolve the simulator to run on: `PO_SIM_DEVICE` env override, else the
 * preferred name, else the first available iOS simulator (`simctl` only
 * exposes iOS devices in its `devices available` listing).
 */
async function resolveDeviceName(): Promise<string> {
  const override = process.env.PO_SIM_DEVICE ?? '';
  const res = await exec('xcrun', ['simctl', 'list', 'devices', 'available'], { timeoutMs: 30_000 });
  const names = [...res.stdout.matchAll(/^\s+(.*?)\s*\([0-9A-F-]{36}\)/gmu)].map((m) => m[1]);
  if (override !== '') {
    if (!names.includes(override)) {
      throw new Error(`PO_SIM_DEVICE=${override} matches no available simulator: ${names.join(', ')}`);
    }
    return override;
  }
  if (names.includes(DEVICE_PREFERENCE)) return DEVICE_PREFERENCE;
  const fallback = names[0];
  if (fallback === undefined) throw new Error('no available iOS simulator');
  return fallback;
}
/** `config::deep_link::PAIR_SCHEME` / `CHAT_SCHEME` — the app's URL scheme. */
const URL_SCHEME = 'po';

/**
 * Pre-approve [`URL_SCHEME`] so `simctl openurl` hands the URL straight to the
 * app. Without it the device raises an interactive `Open in "po"?` alert and
 * the harness — which has no tap channel — cannot answer it.
 *
 * LaunchServices reads this per-device plist at boot, so it must be written
 * while the device is shut down (`setup` orders it that way); writing it in the
 * running session changes nothing until the next boot.
 */
async function approveUrlScheme(udid: string): Promise<void> {
  const plist = join(
    process.env.HOME ?? '',
    'Library/Developer/CoreSimulator/Devices',
    udid,
    'data/Library/Preferences/com.apple.launchservices.schemeapproval.plist',
  );
  // `com.apple.CoreSimulator.CoreSimulatorBridge--><scheme> = <bundle id>`.
  const key = `com.apple.CoreSimulator.CoreSimulatorBridge-->${URL_SCHEME}`;
  await exec('/usr/libexec/PlistBuddy', ['-c', `Delete ${key}`, plist], {
    timeoutMs: 30_000,
    allowFail: true,
  });
  const res = await exec('/usr/libexec/PlistBuddy', ['-c', `Add ${key} string ${BUNDLE_ID}`, plist], {
    timeoutMs: 30_000,
    allowFail: true,
  });
  if (res.code !== 0) {
    throw new Error(`cannot pre-approve the ${URL_SCHEME}:// scheme for ${udid}: ${res.stderr || res.stdout}`);
  }
}

/** Resolve the built ios-sim .app; build with cargo if missing or stale. */
async function resolveApp(ctx: { note(m: string): void }): Promise<string> {
  // A build older than the newest source file is stale: it would silently run
  // yesterday's code while the source moved on. Rebuild it.
  if (existsSync(BUNDLE_BIN) && !isStale(BUNDLE_BIN)) return BUNDLE_DIR;
  ctx.note(existsSync(BUNDLE_BIN) ? 'iOS sim build stale — rebuilding' : 'iOS sim build missing — rebuilding');
  const res = await exec(CARGO, [
    'build',
    '--release',
    '--target',
    IOS_SIM_TARGET,
    '--features',
    `gpui-mobile,${E2E_SEAM_FEATURE}`,
  ], {
    cwd: process.cwd(),
    timeoutMs: 3_600_000,
  });
  if (res.code !== 0) throw new Error(`cargo build ios-sim failed:\n${res.stderr}`);

  const bin = join(process.cwd(), 'target', IOS_SIM_TARGET, 'release', 'po');
  if (!existsSync(bin)) throw new Error(`cargo produced no binary at ${bin}`);

  // Assemble the .app bundle.
  mkdirSync(BUNDLE_DIR, { recursive: true });
  writeFileSync(BUNDLE_BIN, readFileSync(bin));
  chmodSync(BUNDLE_BIN, 0o755);
  writeFileSync(join(BUNDLE_DIR, 'Info.plist'), infoPlist(BUNDLE_ID));
  return BUNDLE_DIR;
}
export async function create(
  runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
  opts: SurfaceOptions,
): Promise<IosSimSurface> {
  const app = await resolveApp(runCtx);
  return new IosSimSurface(app, opts, runCtx);
}

export class IosSimSurface {
  readonly name = 'ios-sim';
  readonly caps = { env: true, deepLink: true };
  readonly build: Dict = {};
  private app: string;
  private opts: SurfaceOptions;
  private runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string };
  private udid?: string;
  private procs = new Map<string, Captured>();

  constructor(
    app: string,
    opts: SurfaceOptions,
    runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
  ) {
    this.app = app;
    this.opts = opts;
    this.runCtx = runCtx;
    this.build['app'] = app;
  }

  get peerIds(): string[] {
    return this.opts.peers;
  }

  private simctl(args: string[], timeoutMs = 120_000) {
    return exec('xcrun', ['simctl', ...args], { timeoutMs });
  }

  async setup(): Promise<void> {
    // One simulator hosts the (single) joiner peer; resolve which one, then
    // boot and install.
    DEVICE = await resolveDeviceName();
    const list = await this.simctl(['list', 'devices', 'available'], 60_000);
    const match = list.stdout.match(new RegExp(`\\(([A-F0-9-]{36})\\) \\(Shutdown\\).*\\n.*${DEVICE}`, 'u'));
    const alt = list.stdout.split('\n').findIndex((l) => l.includes(DEVICE));
    const udid =
      match?.[1] ??
      (alt >= 0 ? list.stdout.slice(0, alt).match(/\(([A-F0-9-]{36})\)\s*$/u) : undefined)?.[1] ??
      list.stdout.match(new RegExp(`${DEVICE} \\(([A-F0-9-]{36})\\)`, 'u'))?.[1];
    if (udid === undefined) throw new Error(`no available simulator named ${DEVICE}:\n${list.stdout}`);
    this.udid = udid;
    this.build['udid'] = udid;
    // Boot from a known state: the scheme approval below only takes effect at
    // boot, so the device must be down while it is written. `bootstatus -b`
    // then boots it and waits; a plain `boot` would fail with SimError 405 on
    // an already-booted device (e.g. one left up by a manual uninstall).
    await this.simctl(['shutdown', udid], 60_000).catch(() => {});
    await approveUrlScheme(udid);
    await this.simctl(['bootstatus', udid, '-b'], 180_000);
    // Apple's IDS daemon crash-loops on this runtime — measured 8 SIGTRAPs in
    // 70 s on a factory-erased device with nothing installed, and it was
    // identical with Device Hub attached (4 in 45 s), so it is not the app, not
    // device state, and not the absence of a UI. It is also not dismissible:
    // `com.apple.CrashReporter DialogType=none` does not stop the host "Problem
    // Report" window, which steals focus mid-run. oo has no iMessage/IDS
    // dependency, so disable the daemon inside the simulator instead: 0 crashes
    // in 70 s afterwards, it survives a reboot, and the app is unaffected.
    await this.simctl(
      ['spawn', udid, 'launchctl', 'disable', 'system/com.apple.identityservicesd'],
      30_000,
    ).catch(() => {});
    await this.simctl(
      ['spawn', udid, 'launchctl', 'bootout', 'system/com.apple.identityservicesd'],
      30_000,
    ).catch(() => {});
    // Start from a clean app state: a remembered session would skip the
    // pairing form, and the previous run's SIMCTL_CHILD_* defaults would leak
    // into this one. Installing first only to uninstall again is pure I/O.
    await this.simctl(['uninstall', udid, BUNDLE_ID], 120_000).catch(() => {});
    await this.simctl(['install', udid, this.app], 300_000);
    // Prime the local-network grant so LAN peers are not silently dropped.
    await this.simctl(['privacy', udid, 'grant', 'network', BUNDLE_ID], 30_000).catch(() => {});
  }

  async launch(peer: string, env: Dict): Promise<void> {
    if (this.udid === undefined) throw new Error('simulator not booted');
    // SIMCTL_CHILD_* prefix passes env into the spawned app process.
    const childEnv: Dict = { PO_E2E: '1', RUST_LOG: 'info', ...env };
    if (peer !== 'a') childEnv['PO_INSTANCE'] = peer;
    /* oxlint-disable no-await-in-loop -- one defaults write per seam, device serial */
    for (const [k, v] of Object.entries(childEnv)) {
      await exec('xcrun', ['simctl', 'spawn', this.udid, 'defaults', 'write', BUNDLE_ID, `SIMCTL_CHILD_${k}`, v], {
        timeoutMs: 30_000,
      }).catch(() => {});
    }
    /* oxlint-enable no-await-in-loop */
    const logPath = join(this.runCtx.dir, 'ios-sim', `${peer}.log`);
    const proc = spawnLogged({
      name: `ios-sim-${peer}`,
      cmd: 'xcrun',
      args: ['simctl', 'launch', '--console', '--terminate-running-process', this.udid, BUNDLE_ID],
      logPath,
      env: {
        ...definedEnv(),
        ...Object.fromEntries(Object.entries(childEnv).map(([k, v]): [string, string] => [`SIMCTL_CHILD_${k}`, v])),
      },
      cwd: process.cwd(),
      mask: this.runCtx.mask.bind(this.runCtx),
    });
    this.procs.set(peer, proc);
  }

  async relaunch(peer: string, env: Dict): Promise<void> {
    await this.stop(peer);
    await this.launch(peer, env);
  }

  /** The peer's own identity, from the app's marker channel (no DOM here). */
  identity(peer: string): Promise<{ username: string; dhtKey: string }> {
    return readIdentityMarker(this.name, peer, () => this.logs(peer), this.opts.timeoutMs);
  }

  /** The generated share payload, from the same marker channel. */
  readShare(peer: string, timeoutMs: number): Promise<{ pin: string; deepLink: string }> {
    return readShareMarker(this.name, peer, () => this.logs(peer), timeoutMs);
  }

  /** Force-stop the peer and relaunch it with `env`. */
  async restart(peer: string, env: Dict): Promise<void> {
    await this.stop(peer);
    await this.launch(peer, env);
  }

  async ready(peer: string): Promise<string> {
    const proc = this.procs.get(peer);
    if (!proc) throw new Error(`ios-sim peer ${peer} not launched`);
    // Soft budget on purpose: a launch that never logs still counts as ready
    // here, so the timeout is caught rather than thrown — the scenario's marker
    // waits are what fail loudly.
    const seen = await waitFor(
      () => (/Network state: started=|event=/u.test(proc.text()) ? true : null),
      { label: `ios-sim/${peer} network state in the console`, timeoutMs: this.opts.timeoutMs, intervalMs: 500 },
    ).catch(() => null);
    return seen === null ? 'app launched (console attached)' : 'Veilid network state logged via simctl launch --console';
  }

  async openUrl(peer: string, url: string): Promise<void> {
    if (this.udid === undefined) throw new Error('simulator not booted');
    // The URL must not race the launch. `simctl openurl` cold-launches the app
    // when nothing is running, and a pending `simctl launch
    // --terminate-running-process` then kills that instance and relaunches it
    // WITHOUT the URL — the app never sees it, while the env seams still
    // carry the payload and make pairing succeed anyway. Wait until the app
    // is up (its first log line) before delivering the link.
    await this.waitForAppStart(peer);
    await this.simctl(['openurl', this.udid, url], 30_000);
  }

  /** Resolves once the app process is running (its console shows startup). */
  private async waitForAppStart(peer: string): Promise<void> {
    const proc = this.procs.get(peer);
    if (!proc) throw new Error(`ios-sim peer ${peer} not launched`);
    await waitFor(() => (proc.text().includes('Veilid API starting up') ? true : null), {
      label: `ios-sim/${peer} app to start (its first console line)`,
      timeoutMs: this.opts.timeoutMs,
      intervalMs: 250,
    });
  }

  async screenshot(peer: string, label: string): Promise<string | null> {
    if (this.udid === undefined) return null;
    const path = surfaceShotPath(this.runCtx.dir, 'ios-sim', peer, label);
    await execBuffer('xcrun', ['simctl', 'io', this.udid, 'screenshot', path], { timeoutMs: 30_000 });
    return path;
  }

  logs(peer: string): string {
    return this.procs.get(peer)?.text() ?? '';
  }

  async stop(peer: string): Promise<void> {
    const proc = this.procs.get(peer);
    if (!proc) return;
    mkdirSync(join(this.runCtx.dir, 'ios-sim'), { recursive: true });
    writeFileSync(proc.logPath, this.runCtx.mask(proc.text()));
    await proc.stop();
    this.procs.delete(peer);
  }

  async teardown(): Promise<void> {
    /* oxlint-disable no-await-in-loop -- peers are stopped one at a time, in order */
    for (const peer of [...this.procs.keys()]) await this.stop(peer);
    /* oxlint-enable no-await-in-loop */
    if (this.udid !== undefined) await this.simctl(['shutdown', this.udid], 60_000).catch(() => {});
  }
}
