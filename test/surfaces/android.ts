// oxlint-disable max-lines, max-classes-per-file -- one file per surface: the harness keeps each device driver whole, and the DevTools client is part of this driver (a second file would split the adb→CDP seam it documents)
// Android surface — adb only: install/am start/logcat/screencap.
// One emulator hosts the peer(s); `am start` cannot set environment variables
// for the app it starts, so the harness seams travel as launch-intent extras
// (`--es po_e2e_<KEY> <value>`, stripped by src/platform/native_bridge.rs and
// read as the same names the environment uses) — nothing here types into the
// app (docs/device-testing.md § Harness seams).
//
// The debuggable WebView serves Chrome DevTools on
// `webview_devtools_remote_<pid>`; `queryDom()` forwards it over adb and
// reads the rendered DOM — the channel that distinguishes a working first
// screen from a frozen one (a screenshot cannot).
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, execBuffer } from '../lib/proc.ts';
import { readIdentityMarker, waitFor } from '../lib/wait.ts';
import { ensureBuilt, E2E_SEAM_FEATURE } from '../lib/build.ts';
import { surfaceShotPath } from '../lib/run.ts';
import type { Dict, SurfaceOptions } from '../lib/types.ts';

const DX = join(process.env.HOME ?? '', '.cargo', 'bin', 'dx');
const BUNDLE_ID = 'com.debanjanbasu.po';
// The launcher activity lives in the Dioxus-generated package, not the app
// id's package: `MainActivity.kt` declares `package dev.dioxus.main`, so the
// manifest names `dev.dioxus.main.MainActivity` under the `com.debanjanbasu.po`
// app id. `${BUNDLE_ID}/.MainActivity` does not exist.
const ACTIVITY = 'dev.dioxus.main.MainActivity';
const AVD = 'Medium_Phone';

/**
 * The APK to install.
 *
 * The **release** build is the one to use: `dx build --release` still emits a
 * gradle `debug` variant, but its `libmain.so` is stripped — 28 MB against
 * the debug build's 952 MB (uncompressed), which does not fit on the
 * emulator's /data alongside anything else.
 *
 * `dx` nests the gradle project one level deeper than a plain `dx build`
 * suggests (`android/app/app/...`), and the depth has changed between dx
 * versions — so look for the APK rather than hardcoding one layout, and fail
 * with the paths that were actually found.
 */
function apkPath(): string {
  const root = join(process.cwd(), 'target', 'dx', 'po');
  const candidates = [
    join(root, 'release', 'android', 'app', 'app', 'build', 'outputs', 'apk', 'debug', 'app-debug.apk'),
    join(root, 'release', 'android', 'app', 'build', 'outputs', 'apk', 'debug', 'app-debug.apk'),
    join(root, 'debug', 'android', 'app', 'app', 'build', 'outputs', 'apk', 'debug', 'app-debug.apk'),
    join(root, 'debug', 'android', 'app', 'build', 'outputs', 'apk', 'debug', 'app-debug.apk'),
  ];
  for (const candidate of candidates) {
    if (existsSync(candidate)) return candidate;
  }
  throw new Error(
    `Android APK not found. Looked in:\n${candidates.map((c) => `  ${c}`).join('\n')}\n` +
      'Run `dx build --release --features e2e-seams --platform android` first.',
  );
}

export async function create(
  runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
  opts: SurfaceOptions,
): Promise<AndroidSurface> {
  const apk = apkPath();
  await ensureBuilt({
    artifact: apk,
    what: 'Android APK',
    note: runCtx.note.bind(runCtx),
    hint: 'check abiFilters vs emulator ABI',
    build: async () => {
      // Release profile: `dx build --release` still emits a gradle `debug`
      // variant (nothing in-repo configures release signing, so AGP would
      // emit app-release-unsigned.apk, which adb install rejects), but its
      // `libmain.so` is stripped — 28 MB against the debug build's 952 MB
      // uncompressed, which does not fit on the emulator's /data. The seam
      // feature compiles in either way, so the harness gets its seams
      // without signing infra.
      const res = await exec(
        DX,
        ['build', '--release', '--features', E2E_SEAM_FEATURE, '--platform', 'android'],
        { cwd: process.cwd(), timeoutMs: 2_400_000 },
      );
      if (res.code !== 0) throw new Error(`dx build android failed:\n${res.stderr}`);
    },
  });
  return new AndroidSurface(apk, opts, runCtx);
}

export class AndroidSurface {
  readonly name = 'android';
  // `env` is true: the seams travel in the launch intent's extras, which the
  // app reads as the same names the environment uses (src/config.rs::e2e).
  readonly caps = { env: true, deepLink: true };
  readonly build: Dict = {};
  private apk: string;
  private opts: SurfaceOptions;
  private runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string };
  private serial?: string;
  private logcat: { lines: string[]; stop(): void } | null = null;
  // The one DevTools page target, connected over adb on demand (see
  // `queryDom`). `null` = not connected yet; a dropped socket goes back to
  // `null`, so the next read re-resolves the pid → socket → port path.
  private page: CdpPage | null = null;

  constructor(
    apk: string,
    opts: SurfaceOptions,
    runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
  ) {
    this.apk = apk;
    this.opts = opts;
    this.runCtx = runCtx;
    this.build['apk'] = apk;
  }

  get peerIds(): string[] {
    return this.opts.peers;
  }

  /** `-s <serial>` once a device is resolved, so a second emulator is never hit. */
  private serialArgs(args: string[]): string[] {
    return this.serial === undefined ? args : ['-s', this.serial, ...args];
  }

  private adb(args: string[], timeoutMs = 120_000) {
    return exec('adb', this.serialArgs(args), { timeoutMs });
  }

  // oxlint-disable-next-line max-lines-per-function -- boot/install readiness is one ordered sequence
  async setup(): Promise<void> {
    // Boot the AVD if no device is attached.
    const devices = await this.adb(['devices'], 30_000);
    const attached = devices.stdout.split('\n').filter((l) => l.endsWith('\tdevice')).length;
    if (attached === 0) {
      this.runCtx.note(`no adb device — booting emulator ${AVD}`);
      spawnEmulator(AVD);
    }
    // Readiness is waited for on the device, not sampled from the host.
    //
    // `adb wait-for-device` blocks until adb has it; the two shell loops below
    // then block until the device itself reports the state we need. The
    // previous form was a host-side `getprop` every 3 s against a deadline:
    // it added up to 3 s of latency to every boot, and — because a restored
    // snapshot already answers `sys.boot_completed=1` — it could also pass
    // while the system was still coming up. The deadlines here are guards
    // against a wedged emulator, not the mechanism.
    await exec('adb', ['wait-for-device'], { timeoutMs: 300_000 });
    // Resolve the serial before either shell loop: they must run against the
    // one device this run owns, and a physical phone can be attached while the
    // AVD is used. `wait-for-device` itself takes no target.
    const serials = await this.adb(['devices'], 30_000);
    const attachedLines = serials.stdout.split('\n').filter((l) => l.endsWith('\tdevice'));
    // Same precedence adb itself uses, so the harness and adb cannot disagree:
    // ANDROID_SERIAL, else the emulator (this surface's documented target),
    // else the only attached device. Taking whichever line adb lists first
    // would point uninstall/install/force-stop at a plugged-in physical phone.
    const chosen =
      process.env.ANDROID_SERIAL ?? attachedLines.find((l) => l.startsWith('emulator-')) ?? attachedLines[0];
    this.serial = chosen?.split('\t')[0];
    if (this.serial === undefined) throw new Error(`no adb device attached:\n${serials.stdout}`);
    if (attachedLines.length > 1) {
      this.runCtx.note(`adb devices: ${attachedLines.length} attached — targeting ${this.serial}`);
    }
    this.build['serial'] = this.serial;
    await this.adb(
      ['shell', 'while [ "$(getprop sys.boot_completed)" != "1" ]; do sleep 1; done'],
      600_000,
    );
    // `sys.boot_completed` is not install-readiness. Ask the package manager
    // that actually serves the install (`pm path android` is the cheapest call
    // that only answers once it is up), and let the device decide when.
    await this.adb(['shell', 'until pm path android >/dev/null 2>&1; do sleep 1; done'], 300_000);
    // A debug APK is ~480 MB; an upgrade install needs room for both copies
    // at once, and the emulator's /data cannot always hold them. Uninstall
    // first — the emulator keeps no state worth keeping between runs, and
    // the app's own data dir is isolated per run anyway (see `launch`).
    await this.adb(['uninstall', BUNDLE_ID], 120_000).catch(() => {});

    // A device that is up can still refuse an install transiently — the
    // package manager restarts, the adb transport drops a beat. Retry only
    // those: anything else (insufficient storage, a signature clash) is a real
    // failure and retrying it just delays the report.
    const TRANSIENT_INSTALL =
      /waiting for device|device offline|device unauthorized|closed|broken pipe|error: device/iu;
    /* oxlint-disable no-await-in-loop -- install retries must run one at a time against the device */
    for (let attempt = 1; ; attempt += 1) {
      const install = await exec('adb', this.serialArgs(['install', '--no-streaming', this.apk]), {
        timeoutMs: 900_000,
        allowFail: true,
      });
      if (install.code === 0) break;
      const text = `${install.stdout}\n${install.stderr}`.trim();
      if (attempt >= 3 || !TRANSIENT_INSTALL.test(text)) {
        throw new Error(`adb install failed (exit ${install.code}, attempt ${attempt}):\n${text}`);
      }
      this.runCtx.note(`adb install attempt ${attempt} hit a transient device state; retrying`);
    }
    /* oxlint-enable no-await-in-loop */
    // Prime the LAN grant without a prompt (docs: pm grant). Without it the
    // activity blocks on the permission dialog and the app never starts.
    await this.adb(['shell', 'pm', 'grant', BUNDLE_ID, 'android.permission.ACCESS_LOCAL_NETWORK'], 30_000).catch(() => {});
  }

  /**
   * The Veilid data dir + namespace for this peer, unique per run.
   *
   * Derived from the run directory's timestamp, so each run gets a fresh
   * identity and a fresh (signed-out) app state.
   */
  private instanceName(peer: string): string {
    const stamp = this.runCtx.dir.split('/').pop() ?? '';
    const suffix = stamp.replace(/[^A-Za-z0-9]/gu, '').slice(-12);
    return `${peer}-${suffix}`;
  }

  async launch(peer: string, env: Dict): Promise<void> {
    // `am start` cannot set environment variables for the app it starts, so
    // the seams travel as intent extras: `--es po_e2e_<KEY> <value>`. The app
    // strips the `po_e2e_` prefix and reads the same names the environment
    // uses (src/config.rs::e2e, native/android/MainActivity.kt).
    //
    // The instance name is unique per run: the emulator keeps the app's data
    // between runs, and a remembered session would restore straight into the
    // signed-in home screen — the pairing form would never appear, and the
    // pairing link would be correctly refused.
    const seams: Dict = { PO_E2E: '1', PO_INSTANCE: this.instanceName(peer), ...env };
    const args = ['shell', 'am', 'start', '-n', `${BUNDLE_ID}/${ACTIVITY}`];
    for (const [key, value] of Object.entries(seams)) {
      if (!key.startsWith('PO_') || value === undefined || value === null) continue;
      args.push('--es', `po_e2e_${key}`, value);
    }
    await this.adb(args, 60_000);
    this.startLogcat();
  }

  /**
   * Force-stop the peer and relaunch it with the same seams — the app reads
   * them from the launch intent, so a relaunch needs them again.
   */
  async restart(peer: string, env: Dict): Promise<void> {
    await this.stop(peer);
    await this.launch(peer, env);
  }

  /**
   * Relaunch with a new set of seams.
   *
   * The native joiner needs the pairing PIN/password at launch time, and
   * Android reads them from the launch intent — so a relaunch carries the
   * new values, not the old ones.
   */
  async relaunch(peer: string, env: Dict): Promise<void> {
    await this.stop(peer);
    await this.launch(peer, env);
  }

  /**
   * The peer's own identity, from the app's marker channel (no DOM here).
   *
   * The `e2e` marker channel is on — the seams travel in the launch intent —
   * so `event=identity` reaches logcat like it does the other native surfaces.
   */
  identity(peer: string): Promise<{ username: string; dhtKey: string }> {
    return readIdentityMarker(this.name, peer, () => this.logs(peer), this.opts.timeoutMs);
  }

  private startLogcat(): void {
    const lines: string[] = [];
    const child = spawn('adb', this.serialArgs(['logcat', '-v', 'time', 'RustStdoutStderr:D', '*:S']));
    child.stdout?.on('data', (chunk: Buffer) => {
      lines.push(chunk.toString('utf8'));
    });
    child.stderr?.on('data', (chunk: Buffer) => {
      lines.push(chunk.toString('utf8'));
    });
    this.logcat = {
      lines,
      stop: () => {
        child.kill('SIGTERM');
      },
    };
  }

  async ready(peer: string): Promise<string> {
    // A 1 s probe cadence: a pid probe that fails is the device refusing, not a
    // race, so there is nothing to retry faster for.
    const pid = await waitFor(
      async () => {
        const out = await this.adb(['shell', 'pidof', BUNDLE_ID], 15_000).catch(() => null);
        return out && out.stdout.trim() !== '' ? out.stdout.trim() : null;
      },
      { label: `android/${peer} app pid (pidof ${BUNDLE_ID})`, timeoutMs: this.opts.timeoutMs, intervalMs: 1000 },
    );
    // A pid is not a first screen: on a cold-booted emulator it appears while
    // the splash is still painting and before the app's own stdout reaches
    // logcat, so a caller that reads a marker right after `ready` saw nothing
    // (two smoke runs, 2026-11-12T094622Z / T101111Z, both against a boot
    // screenshot). Wait for the app's own first output, like the other native
    // surfaces do — soft budget, caught rather than thrown, because the
    // scenario's own marker waits are what fail loudly.
    const seen = await waitFor(
      () => (/Network state: started=|event=/u.test(this.logs(peer)) ? true : null),
      { label: `android/${peer} first app output in logcat`, timeoutMs: this.opts.timeoutMs, intervalMs: 1000 },
    ).catch(() => null);
    // The screenshot is the first-screen evidence.
    await this.screenshot(peer, 'first-screen');
    return seen === null
      ? `app process running (pid ${pid}); no app output in logcat yet; screenshot captured`
      : `app process running (pid ${pid}); app output logged; screenshot captured`;
  }

  async openUrl(_peer: string, url: string): Promise<void> {
    await this.adb(['shell', 'am', 'start', '-a', 'android.intent.action.VIEW', '-d', url, BUNDLE_ID], 60_000);
  }

  async screenshot(peer: string, label: string): Promise<string | null> {
    const path = surfaceShotPath(this.runCtx.dir, 'android', peer, label);
    const png = await execBuffer('adb', this.serialArgs(['exec-out', 'screencap', '-p']), { timeoutMs: 60_000 });
    writeFileSync(path, png);
    return path;
  }

  logs(_peer: string): string {
    return this.logcat?.lines.join('') ?? '';
  }

  /**
   * What `selector` matched in the WebView's rendered DOM, or null.
   *
   * The WebView here is `debuggable` (gradle `debug` variant), so Android
   * exposes its DevTools socket — `webview_devtools_remote_<pid>`, one per
   * app process. This surface reaches it with nothing but adb:
   *
   * 1. `pidof` the app — the socket's name carries the pid, so a stale pid
   *    (force-stop + relaunch, `restart`/`relaunch`) names a socket that is
   *    gone, and the read re-resolves instead of answering from a corpse;
   * 2. `adb forward tcp:0 localabstract:<socket>` — port 0 makes adb print
   *    the host port it picked, so two surface instances (or a leftover
   *    forward) can never collide;
   * 3. `/json/list` → the `type: "page"` target → its `webSocketDebuggerUrl`;
   * 4. one `Runtime.evaluate` over that WebSocket.
   *
   * This is the channel the marker log cannot be: a permanently frozen
   * splash screenshots exactly like a working screen (the 2026-11-13 render
   * wedge: frame 1 painted, every later batch lost — nodes were created in
   * the page but never attached, and the app kept logging), while the DOM
   * read below distinguishes them. Node's own WebSocket client speaks the
   * protocol, so the harness needs no new dependency.
   */
  async queryDom(peer: string, selector: string): Promise<string | null> {
    // The probe returns a JSON *string*: by-value objects come back loosely
    // typed, and a string is narrowed on the host without any type assertion.
    // querySelectorAll is the count channel (0 = absent, the frozen case);
    // a hit reads back what it was so the run's evidence names the element.
    const expression = `(() => {
      const nodes = document.querySelectorAll(${JSON.stringify(selector)});
      const node = nodes[0];
      const probe = node === undefined
        ? { count: 0 }
        : {
            count: nodes.length,
            id: node.id ?? null,
            ariaLabel: node.getAttribute('aria-label'),
            text: (node.textContent ?? '').trim().slice(0, 80),
            connected: node.isConnected,
          };
      return JSON.stringify(probe);
    })()`;
    const result = await this.cdp(peer, expression);
    if (result.type !== 'string' || typeof result.value !== 'string') {
      throw new Error(
        `android/${peer}: Runtime.evaluate returned ${result.type} (${result.description ?? 'no description'}) for ${selector} — the WebView answered but not with the probe JSON`,
      );
    }
    const value = readDomProbe(JSON.parse(result.value));
    if (value === null) {
      throw new Error(
        `android/${peer}: DevTools probe for ${selector} returned unexpected JSON: ${result.value.slice(0, 200)}`,
      );
    }
    if (value.count === 0) return null;
    const parts = [
      value.id === null || value.id === '' ? null : `#${value.id}`,
      value.ariaLabel === null ? null : `[aria-label="${value.ariaLabel}"]`,
      value.text === '' ? null : JSON.stringify(value.text),
    ].filter((part) => part !== null);
    return parts.join(' ') || `count=${value.count}`;
  }

  /**
   * One CDP `Runtime.evaluate` against the app's WebView.
   *
   * The socket is re-resolved after a drop (force-stop + relaunch invalidate
   * the pid → socket → forward path), and the callers poll through
   * `queryDom`, where one iteration costs one `pidof`, one `forward` and one
   * HTTP GET — cheap enough to drive a readiness wait on.
   */
  private async cdp(peer: string, expression: string): Promise<CdpRemoteObject> {
    this.page ??= await this.connectPage(peer);
    try {
      return await this.page.evaluate(expression);
    } catch (err) {
      this.page = null;
      throw new Error(
        `android/${peer}: DevTools socket dropped before the read (${err instanceof Error ? err.message : String(err)})`,
        { cause: err },
      );
    }
  }

  /**
   * Resolve the pid → socket → forward → page WebSocket once, with the pid
   * resolved inside the readiness loop so the poll cannot answer from a
   * previous process: `waitFor` only treats a truthy return as done, so a
   * launch in flight (no pid, no socket yet) keeps the poll alive.
   */
  private async connectPage(peer: string): Promise<CdpPage> {
    const pid = await waitFor(
      async () => {
        const out = await this.adb(['shell', 'pidof', BUNDLE_ID], 15_000).catch(() => null);
        return out && out.stdout.trim() !== '' ? out.stdout.trim() : null;
      },
      { label: `android/${peer} app pid (pidof ${BUNDLE_ID})`, timeoutMs: this.opts.timeoutMs, intervalMs: 1000 },
    );
    const socket = `webview_devtools_remote_${pid}`;
    const port = await this.forwardDevtools(peer, socket);
    const target = await waitFor(() => pageTarget(port), {
      label: `android/${peer} webview DevTools page on :${port} (${socket})`,
      timeoutMs: this.opts.timeoutMs,
      intervalMs: 1000,
    });
    const page = await openPage(peer, target);
    this.runCtx.note(`android/${peer}: CDP over adb — ${socket} → 127.0.0.1:${port} (${target.title})`);
    return page;
  }

  /**
   * `adb forward tcp:0` for the DevTools socket, printing the host port it
   * picked so two instances can never collide, with any stale forward to the
   * same socket dropped first: a forward whose socket died after an earlier
   * call still lists as taken but serves nothing.
   */
  private async forwardDevtools(peer: string, socket: string): Promise<number> {
    const previous = await this.adb(['forward', '--list'], 15_000).catch(() => null);
    if (previous !== null) {
      const stale = previous.stdout
        .split('\n')
        .map((line) => line.trim().split(/\s+/u))
        .filter((parts) => parts.length === 3 && parts[0] === this.serial && parts[2] === `localabstract:${socket}`)
        .map((parts) => parts[1]);
      /* oxlint-disable no-await-in-loop -- adb forwards are removed one at a time, serially against the device */
      for (const local of stale) {
        await this.adb(['forward', '--remove', local], 15_000).catch(() => {});
      }
      /* oxlint-enable no-await-in-loop */
    }
    const forwarded = await exec('adb', this.serialArgs(['forward', 'tcp:0', `localabstract:${socket}`]), {
      timeoutMs: 30_000,
    });
    const port = Number.parseInt(forwarded.stdout.trim(), 10);
    if (!Number.isInteger(port) || port <= 0) {
      throw new Error(
        `android/${peer}: adb forward tcp:0 → ${socket} printed '${forwarded.stdout.trim()}' (${forwarded.stderr.trim()}), not a port`,
      );
    }
    this.build['devtoolsPort'] = String(port);
    return port;
  }

  async stop(_peer: string): Promise<void> {
    // The socket dies with the process: drop it before `force-stop` so a
    // relaunch re-resolves pid → socket → port instead of reading a corpse.
    this.page?.close();
    this.page = null;
    await this.adb(['shell', 'am', 'force-stop', BUNDLE_ID], 30_000).catch(() => {});
  }

  async teardown(): Promise<void> {
    this.page?.close();
    this.page = null;
    this.logcat?.stop();
    const dir = join(this.runCtx.dir, 'android');
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, 'logcat.log'), this.runCtx.mask(this.logs('a')));
    await this.adb(['emu', 'kill'], 30_000).catch(() => {});
  }
}

// ── CDP over adb ─────────────────────────────────────────────────────────────
//
// The Android WebView is Chromium and, in this debuggable build, serves the
// DevTools protocol on a Unix-domain abstract socket. One `Runtime.evaluate`
// per read is the whole protocol this surface needs, so the client below is
// deliberately small: the page-level WebSocket from `/json/list`, requests
// numbered, responses matched by id. Node ships the WebSocket client (global
// since 22), so the harness adds no dependency.

/** One target from `GET /json/list` — only `type: "page"` is used here. */
interface CdpPageTarget {
  id: string;
  type: string;
  title: string;
  url: string;
  webSocketDebuggerUrl: string;
}

/** The `result`/`exceptionDetails` payload of a CDP response message. */
interface CdpRemoteObject {
  type: string;
  value?: unknown;
  description?: string;
}

/** What the in-page probe of `queryDom` reports for one selector. */
interface DomProbe {
  count: number;
  id: string | null;
  ariaLabel: string | null;
  text: string;
  connected: boolean;
}

/**
 * The `type: "page"` target behind a forward's port, or null while the
 * webview is still coming up. Narrowed with a runtime check, so the HTTP
 * response needs no type assertion.
 */
async function pageTarget(port: number): Promise<CdpPageTarget | null> {
  const res = await fetch(`http://127.0.0.1:${port}/json/list`).catch(() => null);
  if (res === null || !res.ok) return null;
  const list: unknown = await res.json().catch(() => null);
  if (!Array.isArray(list)) return null;
  // `Array.isArray` narrows to `any[]` here (unknown → array check), so name
  // the element type once; the guard below does the real narrowing per item.
  const targets = list as unknown[];
  const page = targets.find(
    (target): target is CdpPageTarget =>
      typeof target === 'object' &&
      target !== null &&
      'type' in target &&
      target.type === 'page' &&
      'webSocketDebuggerUrl' in target &&
      typeof target.webSocketDebuggerUrl === 'string',
  );
  return page ?? null;
}

/**
 * Open the page WebSocket and hand back the client. The open wait is
 * event-driven: the surface re-resolves on failure, so a refused socket
 * names its URL in the error and the caller's poll retries.
 */
async function openPage(peer: string, target: CdpPageTarget): Promise<CdpPage> {
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  const { promise: opened, resolve: resolveOpen, reject: rejectOpen } = Promise.withResolvers<void>();
  ws.addEventListener('open', () => {
    resolveOpen();
  }, { once: true });
  ws.addEventListener('error', () => {
    rejectOpen(new Error(`open failed for ${target.webSocketDebuggerUrl}`));
  }, { once: true });
  try {
    await opened;
  } catch (err) {
    throw new Error(
      `android/${peer}: DevTools WebSocket refused (${err instanceof Error ? err.message : String(err)})`,
      { cause: err },
    );
  }
  return new CdpPage(peer, ws);
}

/** The DOM probe's value, narrowed at runtime — or null when it is not one. */
function readDomProbe(value: unknown): DomProbe | null {
  if (typeof value !== 'object' || value === null) return null;
  if ('count' in value && value.count === 0) {
    return { count: 0, id: null, ariaLabel: null, text: '', connected: false };
  }
  if (
    !('count' in value) || typeof value.count !== 'number' ||
    !('id' in value) || typeof value.id !== 'string' && value.id !== null ||
    !('ariaLabel' in value) || typeof value.ariaLabel !== 'string' && value.ariaLabel !== null ||
    !('text' in value) || typeof value.text !== 'string' ||
    !('connected' in value) || typeof value.connected !== 'boolean'
  ) {
    return null;
  }
  return {
    count: value.count,
    id: value.id,
    ariaLabel: value.ariaLabel,
    text: value.text,
    connected: value.connected,
  };
}

/**
 * One connected page-level DevTools socket.
 *
 * `evaluate` serialises its calls on a per-socket queue: CDP answers each
 * request by id, but interleaved sends would pair a caller's await with the
 * wrong response if the caller ever pipelined. The socket never retries — a
 * closed one re-throws and `AndroidSurface.cdp` reconnects from the pid.
 */
class CdpPage {
  private peer: string;
  private ws: WebSocket;
  private nextId = 1;
  private pending = new Map<number, (result: CdpRemoteObject) => void>();
  private closed: Promise<void>;

  constructor(peer: string, ws: WebSocket) {
    this.peer = peer;
    this.ws = ws;
    const { promise, resolve } = Promise.withResolvers<void>();
    this.closed = promise;
    // One close handler per socket: every pending call rejects through
    // `closed` (below), so this does not need the id table.
    ws.addEventListener('close', () => {
      resolve();
    }, { once: true });
    ws.addEventListener('error', () => {
      resolve();
    }, { once: true });
    ws.addEventListener('message', (event: MessageEvent) => {
      if (typeof event.data !== 'string') return;
      const message: unknown = JSON.parse(event.data);
      if (typeof message !== 'object' || message === null) return;
      if (!('id' in message) || !('result' in message)) return;
      const { id, result } = message;
      if (typeof id !== 'number' || typeof result !== 'object' || result === null) return;
      const settle = this.pending.get(id);
      if (settle === undefined) return;
      this.pending.delete(id);
      if ('exceptionDetails' in result && result.exceptionDetails !== undefined) {
        // The probe itself threw inside the page (a malformed selector): name
        // it — a read that cannot parse is a harness bug, not a null count.
        const detail = result.exceptionDetails;
        const cause =
          typeof detail === 'object' && detail !== null && 'exception' in detail &&
          typeof detail.exception === 'object' && detail.exception !== null && 'description' in detail.exception &&
          typeof detail.exception.description === 'string'
            ? detail.exception.description
            : undefined;
        const text = typeof detail === 'object' && detail !== null && 'text' in detail ? String(detail.text) : '';
        throw new Error(
          `${this.peer}: CDP evaluate failed in the page — ${text}${cause === undefined ? '' : `: ${cause}`}`,
        );
      }
      if (!('result' in result)) return;
      const remote = result.result;
      if (typeof remote !== 'object' || remote === null) return;
      if (!('type' in remote) || typeof remote.type !== 'string') return;
      settle({
        type: remote.type,
        value: 'value' in remote ? remote.value : undefined,
        description: 'description' in remote && typeof remote.description === 'string' ? remote.description : undefined,
      });
    });
  }

  /** One `Runtime.evaluate`, as a by-value object. */
  async evaluate(expression: string): Promise<CdpRemoteObject> {
    const id = this.nextId;
    this.nextId += 1;
    const { promise, resolve } = Promise.withResolvers<CdpRemoteObject>();
    this.pending.set(id, resolve);
    this.ws.send(
      JSON.stringify({
        id,
        method: 'Runtime.evaluate',
        params: { expression, returnByValue: true, awaitPromise: false },
      }),
    );
    const raced = await Promise.race([
      promise.then((result) => ({ kind: 'result' as const, result })),
      this.closed.then(() => ({ kind: 'closed' as const })),
    ]);
    this.pending.delete(id);
    if (raced.kind === 'closed') {
      throw new Error(`${this.peer}: DevTools socket closed before the response arrived`);
    }
    return raced.result;
  }

  /** Drop the socket; idempotent, and safe to call after the peer died. */
  close(): void {
    try {
      this.ws.close();
    } catch {
      // Already closed mid-relaunch — nothing to unwind.
    }
  }
}

function spawnEmulator(name: string): void {
  const emu = join(process.env.ANDROID_HOME ?? join(process.env.HOME ?? '', 'Library/Android/sdk'), 'emulator', 'emulator');
  // `-no-snapshot`: a restored snapshot already carries
  // `sys.boot_completed=1`, so readiness probes pass while the package manager
  // is still coming up — that is what let `adb install` race the device and
  // fail with "waiting for device". A cold boot costs ~40 s and is the only
  // state the harness can reason about; the emulator holds nothing worth
  // keeping between runs (its app data is per-run, see `launch`).
  spawn(
    emu,
    ['-avd', name, '-no-window', '-no-snapshot', '-gpu', 'swiftshader_indirect', '-noaudio', '-no-boot-anim', '-camera-back', 'none'],
    { detached: true, stdio: 'ignore' },
  ).unref();
}
