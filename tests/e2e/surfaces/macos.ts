// macOS surface — the built native `po` binary with env passthrough, stdout
// captured, `osascript` deep links (`open location`), and `screencapture` for
// evidence.
//
// Peer a: PO_INSTANCE unset (default data dir). Peer b: PO_INSTANCE=b, so two
// native instances coexist on one machine.
import { join } from 'node:path';
import { exec, spawnLogged, type Captured } from '../lib/proc.ts';
import { ensureBuilt } from '../lib/build.ts';
import { surfaceShotPath } from '../lib/run.ts';
import { waitFor } from '../lib/wait.ts';
import type { Dict, SurfaceOptions } from '../lib/types.ts';

/** Resolve the built native binary; build with cargo if missing or stale. */
async function resolveBinary(ctx: { note(m: string): void }): Promise<string> {
  const bin = join(process.cwd(), 'target', 'release', 'po');
  await ensureBuilt({
    artifact: bin,
    what: 'macOS build',
    note: ctx.note.bind(ctx),
    hint: 'cargo build --release -p po-app produced no binary',
    build: async () => {
      const res = await exec('cargo', ['build', '--release', '-p', 'po-app'], {
        cwd: process.cwd(),
        timeoutMs: 3_600_000,
      });
      if (res.code !== 0) throw new Error(`cargo build --release -p po-app failed:\n${res.stderr}`);
    },
  });
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
    return { ...(instance ? { PO_INSTANCE: instance } : {}), ...extra };
  }

  async launch(peer: string, extraEnv: Dict = {}): Promise<void> {
    const env = this.envFor(peer, extraEnv);
    const log = join(this.runCtx.dir, `${peer}.log`);
    const proc = spawnLogged(this.bin, [], { env, logPath: log });
    this.procs.set(peer, proc);
    this.runCtx.note(`macos: launched peer ${peer} (${this.bin})`);
    await waitFor(async () => proc.running(), { timeoutMs: 5_000, label: `peer ${peer} process alive` });
  }

  async openDeepLink(peer: string, url: string): Promise<void> {
    // Route po:// through LaunchServices to the running app.
    await exec('osascript', [
      '-e',
      `tell application id "com.debanjanbasu.po" to open location "${url}"`,
    ], { timeoutMs: 15_000 });
  }

  async screenshot(peer: string, name: string): Promise<string> {
    const path = surfaceShotPath(this.runCtx, this.name, peer, name);
    await exec('screencapture', ['-x', path], { timeoutMs: 15_000 });
    return path;
  }

  async stop(peer: string): Promise<void> {
    const proc = this.procs.get(peer);
    if (proc) {
      proc.kill();
      this.procs.delete(peer);
    }
  }

  async stopAll(): Promise<void> {
    for (const peer of [...this.procs.keys()]) await this.stop(peer);
  }
}
