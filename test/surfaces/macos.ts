// macOS surface — the built app binary with env passthrough, stdout captured,
// Apple Event deep links (`osascript … open location`), `screencapture` for
// evidence.
//
// Peer a: PO_INSTANCE unset (default data dir). Peer b: PO_INSTANCE=b, so two
// native instances coexist on one machine (`.veilid-b` + namespace).
import { chmodSync, copyFileSync, existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, execBuffer, spawnLogged, type Captured } from '../lib/proc.ts';
import { ensureBuilt, isStale, E2E_SEAM_FEATURE } from '../lib/build.ts';
import { surfaceShotPath } from '../lib/run.ts';
import { readIdentityMarker, readShareMarker, waitFor } from '../lib/wait.ts';
import type { Dict, SurfaceOptions } from '../lib/types.ts';

const CARGO = 'cargo';

/** Resolve the built app bundle; build + assemble with cargo if missing or stale. */
async function resolveBinary(ctx: { note(m: string): void }): Promise<string> {
  // gpui ships a bare executable; a minimal .app bundle (binary + Info.plist)
  // makes LaunchServices able to route po:// Apple Events to THIS binary —
  // a bare process is invisible to `tell application id`.
  const dir = join(process.cwd(), 'target', 'e2e-macos', 'Po.app');
  const bin = join(dir, 'Contents', 'MacOS', 'po');
  const artifact = join(process.cwd(), 'target', 'release', 'po');
  await ensureBuilt({
    artifact,
    what: 'macOS build',
    note: ctx.note.bind(ctx),
    hint: 'cargo build --features gpui-desktop produced no binary',
    build: async () => {
      const res = await exec(CARGO, ['build', '--release', '--features', `${E2E_SEAM_FEATURE},gpui-desktop`], {
        cwd: process.cwd(),
        timeoutMs: 3_600_000,
      });
      if (res.code !== 0) throw new Error(`cargo build desktop failed:\n${res.stderr}`);
    },
  });
  // Assemble the bundle + register it with LaunchServices so po:// URLs reach
  // this binary. Runs even when the artifact was fresh: a missing bundle must
  // still be assembled, and a rebuilt bundle is a new file to LaunchServices
  // until re-registered.
  if (!existsSync(bin) || isStale(bin)) {
    mkdirSync(join(dir, 'Contents', 'MacOS'), { recursive: true });
    copyFileSync(artifact, bin);
    chmodSync(bin, 0o755);
    writeFileSync(
      join(dir, 'Contents', 'Info.plist'),
      '<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict>\n<key>CFBundleIdentifier</key><string>com.debanjanbasu.po</string>\n<key>CFBundleExecutable</key><string>po</string>\n<key>CFBundleName</key><string>po</string>\n<key>CFBundlePackageType</key><string>APPL</string>\n<key>CFBundleShortVersionString</key><string>0.1.0</string>\n<key>CFBundleVersion</key><string>1</string>\n<key>CFBundleURLTypes</key><array><dict>\n<key>CFBundleURLName</key><string>po</string>\n<key>CFBundleURLSchemes</key><array><string>po</string></array>\n</dict></array>\n</dict></plist>\n',
    );
    await exec(
      '/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister',
      ['-f', dir],
      { timeoutMs: 30_000 },
    );
  }
  return bin;
}
export async function create(
  runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
  opts: SurfaceOptions,
): Promise<MacSurface> {
  const bin = await resolveBinary(runCtx);
  return new MacSurface(bin, opts, runCtx);
}

export class MacSurface {
  readonly name = 'macos';
  readonly caps = { env: true, deepLink: true };
  readonly build: Dict = {};
  private bin: string;
  private opts: SurfaceOptions;
  private runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string };
  private procs = new Map<string, Captured>();

  constructor(
    bin: string,
    opts: SurfaceOptions,
    runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
  ) {
    this.bin = bin;
    this.opts = opts;
    this.runCtx = runCtx;
    this.build['binary'] = bin;
  }

  get peerIds(): string[] {
    return this.opts.peers;
  }

  /** Peer a runs with the default data dir; every other peer gets PO_INSTANCE=<id>. */
  private envFor(peer: string, extra: Dict): Dict {
    const instance = peer === 'a' ? undefined : peer;
    // Each run gets its own data root: a peer that keeps its Veilid dir
    // restores the previous run's remembered session and never reaches the
    // pairing form (or re-pairs an already-paired account).
    const dataHome = join(this.runCtx.dir, 'data', peer);
    mkdirSync(dataHome, { recursive: true });
    return {
      ...process.env,
      XDG_DATA_HOME: dataHome,
      PO_E2E: '1',
      RUST_LOG: 'info',
      ...(instance === undefined ? {} : { PO_INSTANCE: instance }),
      ...extra,
    };
  }

  async setup(): Promise<void> {}

  launch(peer: string, env: Dict): Promise<void> {
    const logPath = join(this.runCtx.dir, 'macos', `${peer}.log`);
    const proc = spawnLogged({
      name: `macos-${peer}`,
      cmd: this.bin,
      args: [],
      logPath,
      env: this.envFor(peer, env),
      cwd: process.cwd(),
      mask: this.runCtx.mask.bind(this.runCtx),
    });
    this.procs.set(peer, proc);
    return Promise.resolve();
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
    // Native tracing stdout starts immediately; the Veilid "Network state"
    // INFO line is the first-screen-era signal.
    const proc = this.procs.get(peer);
    if (!proc) throw new Error(`macos peer ${peer} not launched`);
    // Soft budget on purpose: no network line still counts as ready here, so
    // the timeout is caught rather than thrown — the scenario's marker waits
    // are what fail loudly.
    const seen = await waitFor(
      () => (proc.text().includes('Network state: started=') || proc.text().includes('event=') ? true : null),
      { label: `macos/${peer} network state on stdout`, timeoutMs: this.opts.timeoutMs, intervalMs: 500 },
    ).catch(() => null);
    return seen === null
      ? 'app running (no network-state line yet; stdout captured)'
      : 'Veilid network state logged (app running, stdout captured)';
  }

  async openUrl(peer: string, url: string): Promise<void> {
    // The URL must not race the launch: `open location` starts the app itself
    // when it is not running yet, and that launch would not carry the env
    // seams the relaunch just installed. Wait until the app is up first.
    const proc = this.procs.get(peer);
    if (!proc) throw new Error(`macos peer ${peer} not launched`);
    // Soft budget: if the app never logs, the `osascript` below still fails
    // loudly rather than this wait hanging.
    await waitFor(() => (proc.text().includes('Veilid API starting up') ? true : null), {
      label: `macos/${peer} app to start before the deep link`,
      timeoutMs: this.opts.timeoutMs,
      intervalMs: 250,
    }).catch(() => undefined);
    // `open`/`open -a` route po:// through LaunchServices, which picks a
    // handler by bundle id. The iOS simulator build registers the SAME id
    // (com.debanjanbasu.po), so whichever LaunchServices favours wins — the
    // URL then reaches the simulator app and never the macOS peer this
    // surface is watching. The Apple Event path is unambiguous: it targets
    // the app by id and is the same kAEGetURL event the app's handler
    // (DeepLinkURLEventHandler) already receives.
    await exec('osascript', [
      '-e',
      `tell application id "com.debanjanbasu.po" to open location "${url}"`,
    ], { timeoutMs: 15_000 });
  }

  async screenshot(peer: string, label: string): Promise<string | null> {
    const path = surfaceShotPath(this.runCtx.dir, 'macos', peer, label);
    try {
      // Whole-screen capture: window-id lookup needs the app frontmost.
      await execBuffer('screencapture', ['-x', path], { timeoutMs: 15_000 });
      return path;
    } catch {
      return null;
    }
  }

  logs(peer: string): string {
    return this.procs.get(peer)?.text() ?? '';
  }

  /** Persist captured stdout to the evidence dir (called by teardown). */
  async stop(peer: string): Promise<void> {
    const proc = this.procs.get(peer);
    if (!proc) return;
    mkdirSync(join(this.runCtx.dir, 'macos'), { recursive: true });
    writeFileSync(proc.logPath, this.runCtx.mask(proc.text()));
    await proc.stop();
    this.procs.delete(peer);
  }

  async teardown(): Promise<void> {
    /* oxlint-disable no-await-in-loop -- peers are stopped one at a time, in order */
    for (const peer of [...this.procs.keys()]) await this.stop(peer);
    /* oxlint-enable no-await-in-loop */
  }
}
