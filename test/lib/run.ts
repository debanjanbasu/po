// Run directory, run.log, secret masking, marker timeline.

import { appendFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export function repoRoot(): string {
  return resolve(fileURLToPath(new URL('../..', import.meta.url)));
}

export interface TimelineEntry {
  at: string;
  surface: string;
  peer: string;
  kind: string;
  label: string;
  line: string;
}

/**
 * The capabilities a run recorded as skipped, by name ("signIn", "identity").
 *
 * A skipped step asserted NOTHING, so a run's `ok` alone cannot tell coverage
 * from a scenario that never checked its claim — this is what the result
 * carries for a reviewer to read.
 */
export function skippedCapabilities(ctx: RunContext): string[] {
  return ctx.timeline
    .filter((entry) => entry.kind === 'skip')
    .map((entry) => entry.label.replace(/^skipped: /u, ''));
}

export interface RunContext {
  /** Absolute evidence directory for this run. */
  dir: string;
  logPath: string;
  timeline: TimelineEntry[];
  surfaceDir(surface: string): string;
  note(message: string): void;
  mask(text: string): string;
  rel(path: string): string;
  record(entry: Omit<TimelineEntry, 'at'>): void;
}

/**
 * Pairing links embed the PIN, the P2 seam carries a password, and the QR's
 * `link` carries the pairing payload as its last path segment — so all three
 * are masked on the way into any file we write. Evidence is shared.
 *
 * The pairing share URL carries the same payload as its last path segment
 * (`https://po-ai.pages.dev/p/<payload>`), with no `data=`/`link=` prefix to
 * anchor on — so it is masked by its own prefix. Masking the bare *value*
 * instead of the assembled line does nothing: the rules below are
 * prefix-anchored, so a raw PIN or URL passed on its own survives.
 */
export function maskSecrets(text: string): string {
  return text
    .replace(/((?:pin|password|pw)=)([^\s"']+)/giu, '$1***')
    .replace(/(data=)[A-Za-z0-9_%.-]{8,}/gu, '$1***')
    .replace(/(link=)(?:https?|po):\/\/[^\s"']*/gu, '$1***')
    .replace(/(share_url=)(?:https?:\/\/[^\s"']*)?/giu, '$1***')
    .replace(/(https:\/\/po-ai\.pages\.dev\/p\/)[A-Za-z0-9_%.-]{8,}/gu, '$1***');
}

/** Evidence path for one peer's screenshot inside `surface`'s directory. */
export function surfaceShotPath(runDir: string, surface: string, peer: string, label: string): string {
  const dir = join(runDir, surface);
  mkdirSync(dir, { recursive: true });
  return join(dir, `${peer}-${label.replace(/[^a-z0-9]+/giu, '-').toLowerCase()}.png`);
}

export function createRun(rootArg: string, label?: string): RunContext {
  const root = resolve(repoRoot(), rootArg);
  const stamp = new Date().toISOString().replace(/\.\d+/u, '').replace(/:/gu, '');
  const dir = join(root, label !== undefined && label !== '' ? `${stamp}-${label}` : stamp);
  mkdirSync(dir, { recursive: true });
  const logPath = join(dir, 'run.log');
  writeFileSync(logPath, `# po tandem run ${new Date().toISOString()}\n`);

  const ctx: RunContext = {
    dir,
    logPath,
    timeline: [],
    surfaceDir(name: string) {
      const sub = join(dir, name);
      mkdirSync(sub, { recursive: true });
      return sub;
    },
    note(message: string) {
      const line = `[${new Date().toISOString().slice(11, 19)}] ${message}\n`;
      appendFileSync(logPath, line);
      process.stdout.write(line);
    },
    mask: maskSecrets,
    rel(path: string) {
      const r = relative(repoRoot(), path);
      return r.startsWith('..') ? path : r;
    },
    record(entry: Omit<TimelineEntry, 'at'>) {
      ctx.timeline.push({ at: new Date().toISOString(), ...entry });
    },
  };
  return ctx;
}
