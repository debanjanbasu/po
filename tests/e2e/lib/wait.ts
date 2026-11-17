// Waiting on log markers with timeouts that always leave evidence behind.

import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import type { RunContext } from './run.ts';
import type { Dict, Signal, SignalHit, Surface } from './types.ts';

/** Thrown when a marker seam is not present at all (PO_E2E off / release build). */
export class SeamMissing extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'SeamMissing';
  }
}

export function sleep(ms: number): Promise<void> {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
}

/** A probe returns `null`/`undefined`/`false` for "not yet"; any other value — `''` and `0` included — is a result. */
export async function waitFor<T>(
  probe: () => T | null | undefined | false | Promise<T | null | undefined | false>,
  opts: { label: string; timeoutMs: number; intervalMs?: number },
): Promise<T> {
  const deadline = Date.now() + opts.timeoutMs;
  // Default probe cadence for cheap in-process probes (a log read, a DOM
  // count); device probes that cost a round trip pass their own intervalMs.
  const interval = opts.intervalMs ?? 250;
  for (;;) {
    // oxlint-disable-next-line no-await-in-loop -- poll loop: each probe must observe the previous wait
    const value = await probe();
    if (value !== null && value !== undefined && value !== false) return value;
    const left = deadline - Date.now();
    if (left <= 0) throw new Error(`timeout after ${opts.timeoutMs}ms waiting for ${opts.label}`);
    // oxlint-disable-next-line no-await-in-loop -- the poll interval must elapse before the next probe
    await sleep(Math.min(interval, left));
  }
}

export function parseFields(line: string): Dict {
  const out: Dict = {};
  for (const match of stripAnsi(line).matchAll(/([A-Za-z_][A-Za-z0-9_]*)=("[^"]*"|[^\s]+)/gu)) {
    out[match[1]] = match[2].replace(/^"|"$/gu, '');
  }
  return out;
}

/** Strip ANSI colour codes — native logs interleave them inside fields. */
export function stripAnsi(text: string): string {
  // oxlint-disable-next-line no-control-regex -- stripping ANSI SGR codes means matching the ESC (0x1b) control character
  return text.replace(/\x1b\[[0-9;]*m/gu, '');
}

/**
 * Last line on which `event=<marker>` appears as a whole field.
 *
 * Native logs carry ANSI colour codes (including inside the field itself),
 * and the value may be quoted, so the line is stripped before matching.
 */
export function findMarker(text: string, marker: string): string | undefined {
  const re = new RegExp(`event="?${marker}"?(?=\\s|$)`, 'u');
  const lines = stripAnsi(text)
    .split('\n')
    .filter((line) => re.test(line));
  return lines.length > 0 ? lines[lines.length - 1] : undefined;
}

export function hasAnyMarker(text: string): boolean {
  return /event="?[a-z_]+"?(?=\s|$)/mu.test(stripAnsi(text));
}

/**
 * Wait for `event=<marker>` in a peer's log and return the line's fields.
 *
 * Surfaces without a DOM read what they can observe from the marker channel,
 * so this is the shared polling loop for them.
 */
export function waitForMarkerFields(
  text: () => string,
  marker: string,
  timeoutMs: number,
): Promise<Dict> {
  return waitFor<Dict>(
    () => {
      const line = findMarker(text(), marker);
      if (line === undefined) return undefined;
      const fields = parseFields(line);
      return Object.keys(fields).length > 0 ? fields : undefined;
    },
    { label: `marker event=${marker}`, timeoutMs },
  );
}

/**
 * The peer's own identity from the marker channel: `event=identity
 * username=… dht_key=…`. Native surfaces have no DOM, so this is their only
 * source; the web surface reaches the same marker behind a dialog it opens
 * first. Shared because five drivers read it and the field check is the part
 * that must not drift.
 */
export async function readIdentityMarker(
  surface: string,
  peer: string,
  text: () => string,
  timeoutMs: number,
): Promise<{ username: string; dhtKey: string }> {
  const fields = await waitForMarkerFields(text, 'identity', timeoutMs);
  const username = fields['username'];
  const dhtKey = fields['dht_key'];
  if (!username || !dhtKey) {
    throw new Error(`${surface}/${peer}: identity marker has no username/dht_key fields: ${JSON.stringify(fields)}`);
  }
  return { username, dhtKey };
}

/**
 * The generated share payload from the same marker channel: `event=share
 * pin=… deep_link=…`. Native has no DOM to scrape for it.
 */
export async function readShareMarker(
  surface: string,
  peer: string,
  text: () => string,
  timeoutMs: number,
): Promise<{ pin: string; deepLink: string }> {
  const fields = await waitForMarkerFields(text, 'share', timeoutMs);
  const pin = fields['pin'];
  const deepLink = fields['deep_link'];
  if (!pin || !deepLink) {
    throw new Error(`${surface}/${peer}: share marker has no pin/deep_link fields: ${JSON.stringify(fields)}`);
  }
  return { pin, deepLink };
}

export function tail(text: string, lines = 20): string {
  const all = text.split('\n');
  return all.slice(Math.max(0, all.length - lines)).join('\n');
}

/**
 * Wait for `signal` in `peer`'s log. On timeout: screenshot the peer, write the
 * log tail next to it, and throw with both paths — never hangs, never silent.
 */
export async function waitForSignal(
  ctx: RunContext,
  surface: Surface,
  peer: string,
  signal: Signal,
  timeoutMs: number,
): Promise<SignalHit> {
  const started = Date.now();
  const hit = await awaitSignal(ctx, surface, peer, signal, timeoutMs);
  const result: SignalHit = { ...hit, ms: Date.now() - started };
  ctx.record({ surface: surface.name, peer, kind: result.kind, label: signal.label, line: ctx.mask(result.line) });
  return result;
}

/**
 * The wait and its timeout diagnostics. Kept apart so every throw path is a
 * straight line and the caller's hit is provably assigned.
 */
async function awaitSignal(
  ctx: RunContext,
  surface: Surface,
  peer: string,
  signal: Signal,
  timeoutMs: number,
): Promise<Omit<SignalHit, 'ms'>> {
  try {
    return await waitFor<Omit<SignalHit, 'ms'>>(
      () => {
        const line = findMarker(surface.logs(peer), signal.marker);
        if (line === undefined) return undefined;
        return { kind: 'marker' as const, label: signal.label, line, fields: parseFields(line) };
      },
      { label: signal.label, timeoutMs },
    );
  } catch {
    const text = surface.logs(peer);
    const dump = await dumpOnFailure(ctx, surface, peer, text, signal.label);
    if (!hasAnyMarker(text)) {
      throw new SeamMissing(
        `seam missing on ${surface.name}/${peer}: no \`event=\` line reached the log, so event=${signal.marker} never appeared ` +
          `(PO_E2E=1 unset, a release build where the seams compile out, or — on web — localStorage.oo_e2e not set). ` +
          `log: ${ctx.rel(dump.log)} screenshot: ${dump.shot !== null && dump.shot !== '' ? ctx.rel(dump.shot) : 'none'}`,
      );
    }
    throw new Error(
      `timeout after ${timeoutMs}ms on ${surface.name}/${peer}: ${signal.label}\n` +
        `  wanted: marker event=${signal.marker}\n` +
        `  log: ${ctx.rel(dump.log)}${dump.shot !== null && dump.shot !== '' ? `  screenshot: ${ctx.rel(dump.shot)}` : ''}\n` +
        `  --- last 20 log lines ---\n${tail(text, 20)}`,
    );
  }
}

export async function dumpOnFailure(
  ctx: RunContext,
  surface: Surface,
  peer: string,
  text: string,
  label: string,
): Promise<{ log: string; shot: string | null }> {
  const dir = ctx.surfaceDir(surface.name);
  const slug = label.replace(/[^a-z0-9]+/giu, '-').toLowerCase();
  const log = join(dir, `${peer}-${slug}.timeout.txt`);
  writeFileSync(log, `# ${new Date().toISOString()} ${peer} ${label}\n${ctx.mask(text)}`);
  let shot: string | null = null;
  try {
    shot = await surface.screenshot(peer, `${slug}-timeout`);
  } catch {
    shot = null;
  }
  return { log, shot };
}
