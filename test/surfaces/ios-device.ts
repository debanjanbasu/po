// oxlint-disable max-lines -- one file per surface: the harness keeps each device driver whole
// iOS physical-device surface — xtool for signing/install, xcrun devicectl for
// launch/env/screenshot/deep link. One connected iPhone hosts the joiner peer.
//
// Install path mirrors scripts/ios-device-deploy.sh: dx only signs the outer
// .app (the nested DioxusSwiftPlugins frameworks then fail to load on
// device), so xtool re-signs the whole bundle and installs it. xtool also
// namespaces the on-device bundle ID as XTL-<team>.<bundle-id>, so the
// installed ID is resolved from the device rather than assumed.
//
// Env: devicectl passes DEVICECTL_CHILD_* into the app process, so the
// harness seams (PO_E2E, PO_E2E_PAIR_*) work without a typing channel.
//
// Logs: `--console` bridges the app's stdout, which is how the `event=`
// markers reach the harness. The device attaches one console listener at a
// time, so the URL launch stops the old bridge first, keeps `--console` on
// (the app would otherwise start without its env), and reuses the peer's
// launch env (the restarted app must keep the pairing seams).
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { inflateSync } from 'node:zlib';
import { ensureBuilt, E2E_SEAM_FEATURE } from '../lib/build.ts';
import { surfaceShotPath } from '../lib/run.ts';
import { definedEnv, exec, spawnLogged, type Captured } from '../lib/proc.ts';
import { readIdentityMarker, readShareMarker, sleep, waitFor } from '../lib/wait.ts';
import type { Dict, SurfaceOptions } from '../lib/types.ts';

/**
 * True when a PNG is entirely black (or unreadable). A locked phone's
 * framebuffer captures as a valid but 100%-black PNG, so a byte-size check
 * would wave it through as real evidence.
 */
// oxlint-disable-next-line max-lines-per-function -- one PNG scan; splitting it would duplicate the chunk walk
function isBlankImage(path: string): boolean {
	try {
		const buf = readFileSync(path);
		let off = 8;
		let width = 0;
		let height = 0;
		let colorType = 0;
		const idat: Buffer[] = [];
		while (off + 8 <= buf.length) {
			const len = buf.readUInt32BE(off);
			const type = buf.toString('ascii', off + 4, off + 8);
			const data = buf.subarray(off + 8, off + 8 + len);
			if (type === 'IHDR') {
				width = data.readUInt32BE(0);
				height = data.readUInt32BE(4);
				colorType = data[9];
			} else if (type === 'IDAT') {
				idat.push(data);
			} else if (type === 'IEND') {
				break;
			}
			off += 12 + len;
		}
		const channels = { 0: 1, 2: 3, 4: 2, 6: 4 }[colorType];
		if (channels === undefined || width === 0 || height === 0) return true;
		const raw = inflateSync(Buffer.concat(idat));
		const stride = width * channels;
		// Reconstruct only the first pixel per row — enough to spot an all-black
		// frame without decoding the whole image.
		const previous = Buffer.alloc(stride);
		const current = Buffer.alloc(stride);
		let pos = 0;
		for (let y = 0; y < height; y++) {
			const filter = raw[pos++];
			const row = raw.subarray(pos, pos + stride);
			pos += stride;
			for (let x = 0; x < stride; x++) {
				const a = x >= channels ? current[x - channels] : 0;
				const b = previous[x];
				const c = x >= channels ? previous[x - channels] : 0;
				let v = row[x];
				switch (filter) {
					case 1: v = (v + a) & 255; break;
					case 2: v = (v + b) & 255; break;
					case 3: v = (v + ((a + b) >> 1)) & 255; break;
					case 4: {
						const p = a + b - c;
						const pa = Math.abs(p - a);
						const pb = Math.abs(p - b);
						const pc = Math.abs(p - c);
						v = (v + (pa <= pb && pa <= pc ? a : pb <= pc ? b : c)) & 255;
						break;
					}
					default: break;
				}
				current[x] = v;
			}
			for (let c = 0; c < channels; c++) {
				if (current[c] > 8) return false;
			}
			current.copy(previous);
		}
		return true;
	} catch {
		return true;
	}
}

const DX = join(process.env.HOME ?? '', '.cargo', 'bin', 'dx');
const BUNDLE_ID = 'com.debanjanbasu.po';
const XTOOL = 'xtool';
/**
 * The one physical device this repo tests on: iPhone SE (3rd generation).
 * Pinned by UDID because more than one iPhone can be paired to the Mac, and
 * a run must never land on the wrong one.
 */
const DEVICE_UDID = '00008110-001A44C63E10401E';
const DEVICE_NAME = 'iPhone SE (3rd generation)';

interface DeviceInfo {
  udid: string;
  name: string;
  state: string;
}

/** Resolve the pinned physical device; throws when it is not reachable. */
async function resolveDevice(): Promise<DeviceInfo> {
  const res = await exec('xcrun', ['devicectl', 'list', 'devices'], { timeoutMs: 60_000, allowFail: true });
  if (res.code !== 0) throw new Error(`devicectl list devices failed:\n${res.stderr}`);
  const rows = res.stdout.split('\n').filter((line) => line.includes('(UDID)'));
  // Format: "<name>  <UDID> (UDID)  <state>  <model>  <reality>"
  for (const row of rows) {
    if (!row.includes('physical')) continue;
    const udid = row.match(/([A-F0-9-]{25,40}) \(UDID\)/u)?.[1];
    if (udid !== DEVICE_UDID) continue;
    // State reads "unavailable", "available (paired)" or "connected".
    const state = row.match(/\(UDID\)\s+(unavailable|available[^ ]*(?: \([^)]*\))?|connected)/u)?.[1] ?? '';
    const name = row.trim().split(/\s{2,}/u)[0] ?? '';
    if (state === 'unavailable') continue;
    return { udid, name, state };
  }
  const seen = rows
    .map((row) => {
      const udid = row.match(/([A-F0-9-]{25,40}) \(UDID\)/u)?.[1] ?? '?';
      const name = row.trim().split(/\s{2,}/u)[0] ?? '?';
      return `${name} (${udid})`;
    })
    .filter(Boolean)
    .join('; ');
  throw new Error(
    `${DEVICE_NAME} (${DEVICE_UDID}) is not connected — plug it in over USB and unlock it (seen: ${seen || 'none'}). ` +
      'The simulator surface ("ios-sim") covers the same code path without a phone.',
  );
}

/**
 * Identity of a locally built device bundle.
 *
 * Contents are hashed rather than mtime-stamped: dx rewrites the whole bundle
 * on every build, so a copied file's mtime differs even when the code did not.
 *
 * The **plist is part of the identity**, not just the executable: a
 * metadata-only change (a declared usage description, the status-bar flag)
 * leaves `po` byte-identical, and hashing only the binary made the surface skip
 * the install and report "already installed at this build" while the phone kept
 * running the previous bundle — the exact silent-staleness this stamp exists to
 * prevent. Measured 2026-11-09 while adding `UIStatusBarHidden`.
 */
function appStamp(app: string): string {
	try {
		const binary = readFileSync(join(app, 'po'));
		const plist = readFileSync(join(app, 'Info.plist'));
		const digest = createHash('sha256')
			.update(binary)
			.update(plist)
			.digest('hex')
			.slice(0, 16);
		return `${binary.length}-${digest}`;
	} catch {
		return '';
	}
}

/** Where each device's last installed build identity is recorded. */
const INSTALL_STAMP_PATH = join(process.cwd(), 'target', 'dx-e2e', 'installed-ios-device.json');

/** The installed-build record, or `{}` when it is missing or unreadable. */
function readStampFile(): Dict {
  try {
    const raw: unknown = JSON.parse(readFileSync(INSTALL_STAMP_PATH, 'utf8'));
    if (typeof raw !== 'object' || raw === null) return {};
    return Object.fromEntries(
      Object.entries(raw).filter((entry): entry is [string, string] => typeof entry[1] === 'string'),
    );
  } catch {
    // Unreadable record: start a fresh one.
    return {};
  }
}

/**
 * Build the device .app (aarch64-apple-ios) when missing or stale, and
 * normalize it for xtool.
 *
 * The device build goes to its own target dir: dx writes the simulator and the
 * device bundle to the same `target/dx/po/debug/ios/` otherwise, so a device
 * build would clobber the simulator build (and vice versa). dx inserts its own
 * `dx/` segment into the path, so the bundle lands in
 * `target/dx-e2e/dx/po/release/ios/`.
 */
async function resolveApp(ctx: { note(m: string): void }): Promise<string> {
  const base = join(process.cwd(), 'target', 'dx-e2e', 'dx', 'po', 'release', 'ios');
  const app = join(base, 'po.app');
  await ensureBuilt({
    artifact: join(app, 'po'),
    what: 'iOS device build',
    note: ctx.note.bind(ctx),
    hint: 'aarch64-apple-ios triple and scripts/fix-bundle-case.sh',
    build: async () => {
      // No `--device`: dx treats that as "codesign now", and it can only find an
      // Xcode-managed Apple Development identity — which this repo deliberately
      // does not use (xtool owns signing). The device triple alone is unsigned;
      // xtool re-signs the whole bundle on install.
      const res = await exec(DX, ['build', '--release', '--features', E2E_SEAM_FEATURE, '--platform', 'ios', '--target', 'aarch64-apple-ios'], {
        cwd: process.cwd(),
        timeoutMs: 3_600_000,
        env: { ...process.env, CARGO_TARGET_DIR: join(process.cwd(), 'target', 'dx-e2e') },
        allowFail: true,
      });
      if (res.code !== 0) throw new Error(`dx build ios failed:\n${res.stderr}`);
      // dx emits `po.app` directly ([application] name = "po"); the script
      // only injects the iOS launcher icons the plist declares, since dx's
      // `dx build` has no icon pipeline (`[bundle].icon` only affects
      // `dx bundle`).
      const fix = await exec(join(process.cwd(), 'scripts', 'fix-bundle-case.sh'), [base], {
        cwd: process.cwd(),
        timeoutMs: 120_000,
        allowFail: true,
      });
      if (fix.code !== 0) throw new Error(`fix-bundle-case.sh failed:\n${fix.stderr}`);
    },
  });
  return app;
}

export async function create(
  runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
  opts: SurfaceOptions,
): Promise<IosDeviceSurface> {
  const device = await resolveDevice();
  const app = await resolveApp(runCtx);
  return new IosDeviceSurface(app, device, opts, runCtx);
}

export class IosDeviceSurface {
  readonly name = 'ios-device';
  readonly caps = { env: true, deepLink: true };
  readonly build: Dict = {};
  private app: string;
  private device: DeviceInfo;
  private opts: SurfaceOptions;
  private runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string };
  private procs = new Map<string, Captured>();
  /** The env each peer was launched with — the URL launch must reuse it. */
  private envs = new Map<string, Dict>();
  /** On-device bundle ID — xtool namespaces it as XTL-<team>.<bundle-id>. */
  private installedId = BUNDLE_ID;

  constructor(
    app: string,
    device: DeviceInfo,
    opts: SurfaceOptions,
    runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
  ) {
    this.app = app;
    this.device = device;
    this.opts = opts;
    this.runCtx = runCtx;
    this.build['device'] = device.udid;
    this.build['app'] = app;
  }

  get peerIds(): string[] {
    return this.opts.peers;
  }

  private devicectl(args: string[], timeoutMs = 120_000) {
    return exec('xcrun', ['devicectl', ...args], { timeoutMs, allowFail: true });
  }

  /**
   * Pick the transport xtool can use for this device: `--usb` when the
   * cable is plugged in, `--network` when the phone is on Wi-Fi sync.
   *
   * `xtool devices --all` aborts on the *first* stale entry, so each
   * transport is probed separately and the output is matched against this
   * device's UDID — a neighbouring phone's entry must not satisfy the probe.
   *
   * Note: on macOS 15+ the system usbmuxd has no lockdown pair-record store
   * (CoreDevice owns pairing), so `xtool devices --network` cannot complete
   * a Wi-Fi lockdown connection even when the phone advertises
   * `_apple-mobdev2._tcp` and devicectl has a live tunnel. Install therefore
   * needs USB; everything after the install (launch, env, screenshot, logs)
   * works over Wi-Fi through devicectl.
   */
  private async pickTransport(): Promise<'--usb' | '--network' | ''> {
    // Attempts are counted, not timed: each `xtool devices` probe carries its
    // own 30 s exec timeout, so a wall-clock budget would change how many
    // transports get tried while someone plugs the cable in.
    /* oxlint-disable no-await-in-loop -- device probes are ordered: one transport at a time */
    for (let attempt = 0; attempt < 10; attempt++) {
      for (const mode of ['--usb', '--network'] as const) {
        const res = await exec(XTOOL, ['devices', mode, '--no-wait'], { timeoutMs: 30_000, allowFail: true });
        if (res.code === 0 && res.stdout.includes(this.device.udid)) return mode;
      }
      await sleep(3_000);
    }
    /* oxlint-enable no-await-in-loop */
    return '';
  }

  // oxlint-disable-next-line max-lines-per-function -- signing/install/readiness is one ordered sequence
  async setup(): Promise<void> {
    // dx signs only the outer .app; the nested DioxusSwiftPlugins frameworks
    // then fail to load on device. xtool re-signs the whole bundle.
    const res = await exec(XTOOL, ['--version'], { timeoutMs: 30_000, allowFail: true });
    if (res.code !== 0) {
      throw new Error('xtool not available (node scripts/dx-setup.ts installs it; xtool auth login signs in)');
    }
    const auth = await exec(XTOOL, ['auth', 'status'], { timeoutMs: 60_000, allowFail: true });
    if (res.code !== 0 || /logged out/iu.test(auth.stdout)) {
      throw new Error('xtool is logged out — run `xtool auth login` before device runs');
    }
    // The app may already be on the phone from an earlier install. Skipping
    // the install then lets a run proceed over Wi-Fi with the cable
    // unplugged, because only the install step needs xtool's usbmux path (see
    // pickTransport). But "something is installed" is the wrong question —
    // believing it silently ran whatever build happened to be on the phone,
    // hours out of date. Compare build identities; install when they differ.
    const stamp = appStamp(this.app);
    if (await this.findInstalledId()) {
      if (stamp.length > 0 && this.readInstallStamp() === stamp) {
        this.build['ios_install'] = 'skipped (same build)';
        this.runCtx.note('iOS device app already installed at this build — skipping install');
        return;
      }
      this.runCtx.note('iOS device app on the phone is a different build — reinstalling');
    }

    // xtool sees devices through usbmuxd, not CoreDevice's tunnel, and its
    // `--all` mode aborts on the first stale Wi-Fi entry (a muxError from any
    // other phone). So probe each transport and require *this* UDID, then
    // install with `--udid` so a neighbour's entry cannot be selected.
    const mode = await this.pickTransport();
    if (!mode) {
      throw new Error(
        `${DEVICE_NAME} is not reachable for install: no USB or Wi-Fi-sync usbmux entry for ${DEVICE_UDID}. ` +
          "xtool (the only signer for this repo) reaches devices through usbmuxd, and macOS 15+'s usbmuxd " +
          'has no lockdown pair-record store, so the install step needs the cable. Plug it in for the install, ' +
          'then it can run over Wi-Fi.',
      );
    }
    this.build['xtool_mode'] = mode;
    const install = await exec(XTOOL, ['install', mode, '--udid', this.device.udid, this.app], {
      cwd: process.cwd(),
      timeoutMs: 900_000,
      allowFail: true,
    });
    if (install.code !== 0) {
      // xtool reports progress and errors on stdout — a failure here that
      // carried only stderr produced an empty message, and diagnosing it meant
      // re-running the install by hand to see what it actually said.
      throw new Error(
        `xtool install failed (exit ${install.code}):\n${install.stdout}\n${install.stderr}`,
      );
    }
    this.build['ios_install'] = 'installed';
    this.writeInstallStamp(stamp);
    // Querying apps needs the developer disk image mounted, which a locked
    // phone refuses — poll while the user unlocks. Soft budget: the install
    // already succeeded, so an unresolved bundle id only costs the evidence
    // line set below.
    const found = await waitFor(() => this.findInstalledId(), {
      label: 'ios-device installed bundle id (unlock the phone)',
      timeoutMs: 60_000,
      intervalMs: 3_000,
    }).catch(() => false);
    if (!found) this.build['bundle_id'] = this.installedId;
  }

  /** The build identity this device was last installed with, if recorded. */
  private readInstallStamp(): string | null {
    return readStampFile()[this.device.udid] ?? null;
  }

  /** Record the build identity just installed, per device. */
  private writeInstallStamp(stamp: string): void {
    const all = readStampFile();
    all[this.device.udid] = stamp;
    try {
      mkdirSync(join(process.cwd(), 'target', 'dx-e2e'), { recursive: true });
      writeFileSync(INSTALL_STAMP_PATH, `${JSON.stringify(all, null, 1)}\n`);
    } catch {
      // Failing to persist only costs a reinstall next run.
    }
  }

  /**
   * Resolve the on-device bundle ID. xtool namespaces it as
   * `XTL-<team>.com.debanjanbasu.po`, so it must never be hardcoded.
   * Returns false when the app is not installed (or the phone is locked).
   */
  private async findInstalledId(): Promise<boolean> {
    const apps = await this.devicectl(['device', 'info', 'apps', '--device', this.device.udid], 60_000);
    const found = apps.stdout.match(new RegExp(`[A-Za-z0-9.-]*${BUNDLE_ID.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&')}`, 'u'))?.[0];
    if (found === undefined) return false;
    this.installedId = found;
    this.build['bundle_id'] = found;
    return true;
  }

  /**
   * The Veilid data dir + namespace for this peer, unique per run.
   *
   * The phone keeps its app container between runs, so a fixed `PO_INSTANCE`
   * inherits the previous run's remembered session: the app restores straight
   * into the signed-in home screen and never shows the pairing form, and the
   * pairing link is then refused because the peer is already paired. A fresh
   * instance name per run gives the phone the same clean slate the simulator
   * gets from its uninstall.
   */
  private instanceName(peer: string): string {
    const stamp = this.runCtx.dir.split('/').pop() ?? '';
    // The WHOLE run-dir name, never a tail of it: the tail is the scenario +
    // surface trailer ("…S1-web+ios-device" → "webiosdevice"), identical for
    // every run of the same surface, so the phone would inherit the previous
    // run's paired session and refuse the pairing link.
    const unique = stamp.replace(/[^A-Za-z0-9]/gu, '');
    return `${peer}-${unique}`;
  }

  async launch(peer: string, env: Dict): Promise<void> {
    // A locked phone refuses the launch with FBSOpenApplicationServiceErrorDomain
    // "Locked", and the console bridge just dies — a silent failure. Check first.
    const lock = await this.devicectl(['device', 'info', 'lockState', '--device', this.device.udid], 45_000);
    if (/passcodeRequired:\s*true/u.test(lock.stdout)) {
      throw new Error(
        `${DEVICE_NAME} is locked — unlock it on the device (Face ID or passcode) before the run. ` +
          'devicectl cannot launch an app on a locked phone, and iPhone Mirroring pauses as soon as it locks.',
      );
    }
    // devicectl passes DEVICECTL_CHILD_* into the app process.
    const childEnv: Dict = { PO_E2E: '1', RUST_LOG: 'info', ...env };
    if (peer !== 'a') childEnv['PO_INSTANCE'] = this.instanceName(peer);
    // The URL launch replaces this process, so it has to carry the same env
    // or the restarted app loses the pairing seams (PIN/password) and sits on
    // an empty form.
    this.envs.set(peer, childEnv);
    const logPath = join(this.runCtx.dir, 'ios-device', `${peer}.log`);
    const proc = spawnLogged({
      name: `ios-device-${peer}`,
      cmd: 'xcrun',
      args: [
        'devicectl',
        'device',
        'process',
        'launch',
        '--console',
        '--terminate-existing',
        '--device',
        this.device.udid,
        this.installedId,
      ],
      logPath,
      env: {
        ...definedEnv(),
        ...Object.fromEntries(Object.entries(childEnv).map(([k, v]): [string, string] => [`DEVICECTL_CHILD_${k}`, v])),
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
    if (!proc) throw new Error(`ios-device peer ${peer} not launched`);
    // The failed-launch branch is part of the probe, not a post-hoc check:
    // devicectl exits non-zero when the launch is refused (locked phone, bad
    // bundle id), and without it the wait would sit until the timeout and then
    // report success. The timeout itself is a soft budget, caught below.
    const state = await waitFor(
      () => {
        const text = proc.text();
        if (/Network state: started=|event=/u.test(text)) return 'network' as const;
        if (/error:|failed to launch|could not be completed/iu.test(text)) return 'failed' as const;
        return null;
      },
      { label: `ios-device/${peer} launch on the console`, timeoutMs: this.opts.timeoutMs, intervalMs: 500 },
    ).catch(() => null);
    if (state === 'failed') throw new Error(`ios-device peer ${peer} failed to launch:\n${proc.text().slice(-800)}`);
    return state === 'network'
      ? 'Veilid network state logged via devicectl --console'
      : 'app launched (console attached)';
  }

  // oxlint-disable-next-line max-lines-per-function -- bridge restart + readiness is one ordered sequence
  async openUrl(peer: string, url: string): Promise<void> {
    // devicectl has no openurl; launching with --payload-url hands the URL
    // to the app through the system open-URL path. `--console` must stay on:
    // without it this launch replaces the console-attached instance with a
    // silent one, and the harness would then wait forever for markers that
    // the new process never writes (it also loses the DEVICECTL_CHILD_* env,
    // so PO_E2E would be off).
    //
    // The previous bridge has to go first: the device attaches one console
    // listener at a time, so a second `--console` launch while the first is
    // still attached produces an app whose stdout never reaches either
    // bridge. Stopping the old bridge does not kill the app — the
    // `--terminate-existing` below does that.
    await this.stop(peer);
    const proc = spawnLogged({
      name: `ios-device-${peer}-url`,
      cmd: 'xcrun',
      args: [
        'devicectl',
        'device',
        'process',
        'launch',
        '--console',
        '--terminate-existing',
        '--payload-url',
        url,
        '--device',
        this.device.udid,
        this.installedId,
      ],
      logPath: join(this.runCtx.dir, 'ios-device', `${peer}-url.log`),
      env: {
        ...definedEnv(),
        ...Object.fromEntries(
          Object.entries(this.envs.get(peer) ?? { PO_E2E: '1', RUST_LOG: 'info' }).map(
            ([k, v]): [string, string] => [`DEVICECTL_CHILD_${k}`, v],
          ),
        ),
      },
      cwd: process.cwd(),
      mask: this.runCtx.mask.bind(this.runCtx),
    });
    // The URL launch replaces the peer's app process, so the original
    // console bridge ends; point the peer at the new one.
    this.procs.set(peer, proc);
    // Wait until the new bridge has actually attached and the app is
    // producing output. `--terminate-existing` kills the previous app
    // process, and the replacement needs a moment to boot: a scenario that
    // reads the log before then sees an empty stream and reports a missing
    // seam.
    // Soft budget (the surface's own 120 s default): a run that gets here has
    // already relaunched, so a bridge that never attaches is reported by the
    // scenario's marker wait, not by this one.
    await waitFor(() => (/event=|Network state: started=/u.test(proc.text()) ? true : null), {
      label: `ios-device/${peer} new console bridge to attach`,
      timeoutMs: 120_000,
      intervalMs: 500,
    }).catch(() => undefined);
  }

  async screenshot(peer: string, label: string): Promise<string | null> {
    const path = surfaceShotPath(this.runCtx.dir, 'ios-device', peer, label);
    const res = await this.devicectl(
      ['device', 'capture', 'screenshot', '--device', this.device.udid, '--destination', path],
      60_000,
    );
    if (res.code !== 0) return null;
    // A locked phone's framebuffer is empty: devicectl writes a valid PNG
    // that is 100% black. Shipping that as evidence is worse than no file —
    // it looks like a real capture. Detect it and say so.
    if (isBlankImage(path)) {
      this.runCtx.note(
        `${peer} screenshot "${label}" is blank (phone locked or display asleep) — ` +
          'unlock the iPhone and re-run for visual evidence',
      );
      return null;
    }
    return path;
  }

  logs(peer: string): string {
    // The URL launch opens a second console bridge (it replaces the app
    // process), so its output is a separate stream; the peer may still be
    // carrying markers from before the relaunch, and both streams belong to
    // the same peer.
    const current = this.procs.get(peer)?.text() ?? '';
    const urlLog = join(this.runCtx.dir, 'ios-device', `${peer}-url.log`);
    try {
      return `${readFileSync(urlLog, 'utf8')}\n${current}`;
    } catch {
      return current;
    }
  }

  async stop(peer: string): Promise<void> {
    const proc = this.procs.get(peer);
    if (!proc) return;
    mkdirSync(join(this.runCtx.dir, 'ios-device'), { recursive: true });
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
