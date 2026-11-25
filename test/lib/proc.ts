// Child-process helpers: capture a long-running app's stdout into a log file,
// and run one-shot commands (simctl/adb/open/screencapture) with a timeout.

import { execFile, spawn } from 'node:child_process';
import { appendFileSync, mkdirSync } from 'node:fs';
import { dirname } from 'node:path';
import type { Dict } from './types.ts';

export interface Captured {
  logPath: string;
  text(): string;
  stop(timeoutMs?: number): Promise<void>;
  readonly pid?: number;
}

export interface ExecResult {
  code: number;
  stdout: string;
  stderr: string;
}

/** `process.env` with undefined values dropped — an env channel is a `Dict`. */
export function definedEnv(): Dict {
  return Object.fromEntries(
    Object.entries(process.env).filter((entry): entry is [string, string] => entry[1] !== undefined),
  );
}

export function spawnLogged(opts: {
  name: string;
  cmd: string;
  args: string[];
  logPath: string;
  env?: Dict;
  cwd?: string;
  mask?: (text: string) => string;
}): Captured {
  mkdirSync(dirname(opts.logPath), { recursive: true });
  appendFileSync(opts.logPath, `# ${new Date().toISOString()} spawn ${opts.name}: ${opts.cmd} ${opts.args.join(' ')}\n`);
  const child = spawn(opts.cmd, opts.args, {
    env: opts.env,
    cwd: opts.cwd,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let buffer = '';
  const mask = opts.mask ?? ((text: string) => text);
  const sink = (chunk: Buffer) => {
    const text = chunk.toString('utf8');
    buffer += text;
    appendFileSync(opts.logPath, mask(text));
  };
  child.stdout?.on('data', sink);
  child.stderr?.on('data', sink);
  child.on('error', (err) => { sink(Buffer.from(`# spawn error: ${err.message}\n`)); });

  return {
    logPath: opts.logPath,
    pid: child.pid,
    text: () => buffer,
    stop: (timeoutMs = 4000) => {
      const { promise, resolve } = Promise.withResolvers<void>();
      if (child.exitCode !== null || child.signalCode !== null) { resolve(); return promise; }
      const done = () => { resolve(); };
      // Event-driven, not a sleep: this resolves on the child's `exit`. The
      // timer below is only the SIGTERM→SIGKILL escalation guard.
      child.once('exit', done);
      child.kill('SIGTERM');
      setTimeout(() => {
        if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
        resolve();
      }, timeoutMs);
      return promise;
    },
  };
}

export function exec(
  cmd: string,
  args: string[],
  opts: { env?: Dict; cwd?: string; timeoutMs?: number; allowFail?: boolean } = {},
): Promise<ExecResult> {
  // The promise now exists before `execFile` runs, so a *synchronous* throw
  // (a null byte or a non-string argument) reaches the caller as a throw
  // rather than a rejection — a `.catch()` guard would not swallow it.
  const { promise, resolve, reject } = Promise.withResolvers<ExecResult>();
  execFile(
    cmd,
    args,
    { env: opts.env, cwd: opts.cwd, timeout: opts.timeoutMs ?? 120_000, maxBuffer: 64 * 1024 * 1024 },
    (error, stdout, stderr) => {
      const code = error && typeof error.code === 'number' ? error.code : error ? 1 : 0;
      const result: ExecResult = { code, stdout, stderr };
      if (error && opts.allowFail !== true) {
        reject(new Error(`${cmd} ${args.join(' ')} failed (${code}): ${result.stderr || error.message}`));
        return;
      }
      resolve(result);
    },
  );
  return promise;
}

/** Same as `exec`, but returns raw bytes (adb screencap). */
export function execBuffer(
  cmd: string,
  args: string[],
  opts: { env?: Dict; timeoutMs?: number } = {},
): Promise<Buffer> {
  const { promise, resolve, reject } = Promise.withResolvers<Buffer>();
  execFile(
    cmd,
    args,
    { env: opts.env, timeout: opts.timeoutMs ?? 120_000, maxBuffer: 64 * 1024 * 1024, encoding: 'buffer' },
    (error, stdout, stderr) => {
      if (error) {
        reject(new Error(`${cmd} ${args.join(' ')} failed: ${stderr?.toString() ?? error.message}`));
        return;
      }
      resolve(stdout);
    },
  );
  return promise;
}
