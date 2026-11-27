#!/usr/bin/env node
//
// Set up everything the locked `dx` needs to build this app — web, desktop,
// Android and iOS — on macOS, Linux, and Windows. Idempotent; re-run after
// Android Studio / SDK / Xcode / dx updates. TypeScript + async so the
// independent halves (Android chain, Rust components/targets, dx tooling,
// platform extras) run concurrently instead of one after another.
//
// Android — dx reads the toolchain only from env vars (cli src/build/android.rs
// `AndroidTools::current`): SDK root ANDROID_SDK_ROOT -> ANDROID_SDK ->
// ANDROID_HOME (no default path), NDK NDK_HOME -> ANDROID_NDK_HOME -> newest dir
// under $SDK/ndk, JAVA_HOME as-is, adb from $SDK/platform-tools else PATH. It then
// hard-requires <NDK>/toolchains/llvm/prebuilt/<host>/bin/
// aarch64-linux-android<min_sdk>-clang[.cmd], so the NDK must ship clang wrappers
// up to Dioxus.toml's [android] min_sdk (37 today: NDK r30+, r29 stops at 35).
// Newest packages are installed and that wrapper is asserted here, with the repair
// command, instead of failing later inside `dx`.
//
// Java — newest OpenJDK the host package manager offers, with JAVA_HOME on the
// newest major Gradle accepts: 17-27 (Gradle 9.8.x's window — it refuses 28+, so the
// ceiling tracks Gradle, not Java; AGP 9.5.0 is verified on 27). macOS/Linux install
// the keg the ceiling names; Windows gets Microsoft's build.
//
// Usage:
//   eval "$(node scripts/dx-setup.ts)"    # POSIX: install + export now
//   node scripts/dx-setup.ts              # any OS: install + print exports
//   node scripts/dx-setup.ts --yes        # also apply the shell/registry config
//   node scripts/dx-setup.ts --self-check # assert the platform maps, no I/O
// Progress to stderr, exports to stdout, so `eval` stays clean. The printed block
// and the persisted one are the same text. The JDK and NDK resolve at shell start;
// re-run after dx updates to refresh its versioned wasm tool directories.
// POSIX: writes $XDG_CONFIG_HOME/po/env.sh and sources it from the startup file
// each shell needs (zsh: .zshenv + .zprofile, since macOS /etc/zprofile's
// path_helper reorders PATH; bash: .bashrc and .bash_profile; other: .profile).
// Existing assignments our block overrides are reported. Windows: WebView2 via
// winget, then `setx` for the variables plus the user PATH.
// It asks before changing your shell/registry unless --yes is given.
/* oxlint-disable max-lines -- one directly-run orchestrator: the sections share module state (os, env, brewPrefix, android, failures) and it is invoked as `node scripts/dx-setup.ts` / `eval "$(node scripts/dx-setup.ts)"`. */
// Overrides via environment: ANDROID_HOME, JAVA_HOME. Run from the repo
// root (API levels come from Dioxus.toml). Not covered: emulator/AVD setup, IDE
// extensions (dx doctor lists those), iOS simulator runtimes.

import { spawn } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { dirname, join, resolve, win32 } from 'node:path';
import { createInterface } from 'node:readline/promises';
import { parseArgs } from 'node:util';

type OsName = 'darwin' | 'linux' | 'win32';
interface RunResult {
  status: number;
  stdout: string;
  stderr: string;
}

const JDK_MIN = 17;
// Gradle's ceiling, not Java's: 9.8.x runs on 17-27 and refuses 28+
const JDK_MAX = 27;
// winget's set is narrower than brew/apt
const WINGET_JDK = 'Microsoft.OpenJDK.25';
const WINGET_WEBVIEW = 'Microsoft.EdgeWebView2Runtime';
const SDKMANAGER_MAIN = 'com.android.sdklib.tool.sdkmanager.SdkManagerCli';
const BREW_FAST = { ...process.env, HOMEBREW_NO_AUTO_UPDATE: '1', HOMEBREW_NO_ENV_HINTS: '1' };
const LINUX_DESKTOP_LIBS = ['webkit2gtk-4.1', 'gtk+-3.0', 'alsa', 'libudev', 'ayatana-appindicator3-0.1', 'glib-2.0'];
const APT_DESKTOP_PACKAGES =
  'libwebkit2gtk-4.1-dev libgtk-3-dev libasound2-dev libudev-dev libayatana-appindicator3-dev libglib2.0-dev';
const flags = parseArgs({ options: { yes: { type: 'boolean' }, 'self-check': { type: 'boolean' } } }).values;
const ASSUME_YES = flags.yes === true;

const log = (msg: string): void => void process.stderr.write(`\x1b[2m==> ${msg}\x1b[0m\n`);
/** Task-prefixed log, so concurrent sections stay readable. */
const tagged = (name: string) => (msg: string) =>{  log(`${name}: ${msg}`); };

function fail(msg: string): never {
  process.stderr.write(`ERROR: ${msg}\n`);
  process.exit(1);
}

// ── Async process helpers ───────────────────────────────────────────────────

function run(cmd: string, args: string[], opts: { input?: string; quiet?: boolean; env?: NodeJS.ProcessEnv } = {}): Promise<RunResult> {
  const { promise, resolve: settle } = Promise.withResolvers<RunResult>();
  const child = spawn(cmd, args, { env: opts.env ?? process.env, stdio: 'pipe' });
  let stdout = '';
  let stderr = '';
  child.stdout?.on('data', (chunk: Buffer) => { stdout += chunk.toString(); });
  child.stderr?.on('data', (chunk: Buffer) => {
    stderr += chunk.toString();
    if (opts.quiet !== true) process.stderr.write(chunk);
  });
  child.on('error', (error) => { settle({ status: -1, stdout, stderr: `${stderr}${error.message}` }); });
  child.on('close', (status) => { settle({ status: status ?? -1, stdout, stderr }); });
  if (opts.input !== undefined) child.stdin?.end(opts.input);
  return promise;
}

/** Inherits stdio: for installers whose progress the user should see. */
function runVisible(cmd: string, args: string[], opts: { env?: NodeJS.ProcessEnv } = {}): Promise<number> {
  const { promise, resolve: settle } = Promise.withResolvers<number>();
  const child = spawn(cmd, args, { env: opts.env ?? process.env, stdio: 'inherit' });
  child.on('error', () => { settle(-1); });
  child.on('close', (status) => { settle(status ?? -1); });
  return promise;
}

const hasCommand = async (cmd: string): Promise<boolean> => (await run(cmd, ['--version'], { quiet: true })).status !== -1;

// brew mutates a shared Cellar: queries may race, installs/upgrades may not.
let brewQueue: Promise<unknown> = Promise.resolve();
const brewQuery = (args: string[]) => run('brew', args, { env: BREW_FAST, quiet: true });
function brewInstall(args: string[]): Promise<number> {
  const next = brewQueue.then(() => runVisible('brew', args, { env: BREW_FAST }));
  brewQueue = next.catch(() => undefined);
  return next;
}

// ── Platform maps. Pure and host-independent: --self-check asserts the Windows
//    ones on any machine, and win32.join keeps them independent of the host. ───

function sdkDefault(osName: OsName, env: NodeJS.ProcessEnv, home: string): string {
  if (osName === 'darwin') return join(home, 'Library', 'Android', 'sdk');
  if (osName === 'linux') return join(home, 'Android', 'Sdk');
  return win32.join(env.LOCALAPPDATA ?? win32.join(home, 'AppData', 'Local'), 'Android', 'Sdk');
}

function prebuiltDir(osName: OsName): string {
  // dx hardcodes this, even on arm64 hosts
  if (osName === 'darwin') return 'darwin-x86_64';
  if (osName === 'linux') return 'linux-x86_64';
  return 'windows-x86_64';
}

/** Keep the same DX_HOME fallback as the locked CLI's Workspace::dioxus_data_dir. */
function dxHome(osName: OsName, environment: NodeJS.ProcessEnv, homeDir: string): string {
  if (environment.DX_HOME !== undefined && environment.DX_HOME !== '') return environment.DX_HOME;
  if (osName === 'win32') return win32.join(homeDir, '.dx');
  return osName === 'linux'
    ? join(environment.XDG_DATA_HOME ?? join(homeDir, '.local', 'share'), '.dx')
    : join(homeDir, '.dx');
}

function executableName(osName: OsName, name: string): string {
  return `${name}${osName === 'win32' ? '.exe' : ''}`;
}

/** Numeric compare of the version tail, so package ids ('platforms;android-37.2',
 *  'build-tools;37.0.0', 'openjdk@25') and bare versions order the same way. */
function compareVersions(a: string, b: string): number {
  const tail = (value: string): number[] =>
    (value.replace(/[^0-9.]+/gu, ' ').trim().split(' ').at(-1) ?? '0').split('.').map(Number);
  const pa = tail(a);
  const pb = tail(b);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const diff = (pa[i] ?? 0) - (pb[i] ?? 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

function newest<T>(items: T[], key: (item: T) => string): T | undefined {
  return items.reduce<T | undefined>((best, item) => (best === undefined || compareVersions(key(item), key(best)) > 0 ? item : best), undefined);
}

/** `engines.node` floor as [major, minor] — package.json is its only home. */
function nodeFloor(spec: string): [number, number] {
  const found = /^>=\s*(\d+)\.(\d+)/u.exec(spec) ?? fail(`unsupported engines.node range: ${spec}`);
  return [Number(found[1]), Number(found[2])];
}

/** JDK homes the Windows installers use, newest major first. */
function windowsJdkHomes(env: NodeJS.ProcessEnv, home: string): string[] {
  const roots = [
    'C:\\Program Files\\Microsoft',
    win32.join(env.LOCALAPPDATA ?? win32.join(home, 'AppData', 'Local'), 'Programs', 'Eclipse Adoptium'),
    'C:\\Program Files\\Eclipse Adoptium',
    'C:\\Program Files\\Java',
  ];
  const name = (path: string): string => path.split(/[\\/]/u).at(-1) ?? '';
  return roots
    .flatMap((root) => {
      try {
        return readdirSync(root, { withFileTypes: true })
          .filter((entry) => entry.isDirectory() && /jdk-?\d+/iu.test(entry.name))
          .map((entry) => win32.join(root, entry.name));
      } catch {
        return [];
      }
    })
    .sort((a, b) => compareVersions(name(b).match(/[\d.]+/u)?.[0] ?? '0', name(a).match(/[\d.]+/u)?.[0] ?? '0'));
}

// ── --self-check: no I/O, asserts the maps the host cannot exercise ──────────

if (flags['self-check'] === true) {
  const checks: [boolean, string][] = [
    [sdkDefault('win32', { LOCALAPPDATA: 'C:\\Local' }, 'C:\\Users\\x') === 'C:\\Local\\Android\\Sdk', 'windows SDK default'],
    [sdkDefault('linux', {}, '/home/x') === '/home/x/Android/Sdk', 'linux SDK default'],
    [prebuiltDir('win32') === 'windows-x86_64' && win32.sep === '\\' && win32.delimiter === ';', 'windows paths'],
    [dxHome('win32', {}, 'C:\\Users\\x') === 'C:\\Users\\x\\.dx', 'windows DX_HOME default'],
    [dxHome('linux', {}, '/home/x') === '/home/x/.local/share/.dx', 'linux DX_HOME default'],
    [dxHome('linux', { XDG_DATA_HOME: '/xdg' }, '/home/x') === '/xdg/.dx', 'linux XDG data home'],
    [executableName('win32', 'wasm-bindgen') === 'wasm-bindgen.exe' && executableName('linux', 'wasm-opt') === 'wasm-opt', 'tool executable names'],
    [compareVersions('37.2', '37.1') > 0 && compareVersions('10.0.0', '9.0.0') > 0, 'numeric version order'],
    [compareVersions('platforms;android-37.2', 'platforms;android-37') > 0, 'package-id version order'],
    [newest(['1.0', '10.0', '2.0'], String) === '10.0', 'newest picks numerically'],
    [new RegExp('^ *compile_sdk *= *(\\d+)', 'mu').exec('x\ncompile_sdk = 37\n')?.[1] === '37', 'toml number parse'],
    [nodeFloor('>=26.13.0').join('.') === '26.13', 'engines.node floor parse'],
  ];
  const failed = checks.filter(([ok]) => !ok);
  if (failed.length) fail(`self-check failed: ${failed.map(([, what]) => what).join(', ')}`);
  log('self-check OK');
  process.exit(0);
}

// ── Environment ─────────────────────────────────────────────────────────────

// The floor is read from package.json rather than restated here: npm enforces
// `engines.node` on `npm ci`, so a dev below it would pass setup and then fail
// the install.
const manifest: unknown = JSON.parse(readFileSync('package.json', 'utf8'));
const engines = typeof manifest === 'object' && manifest !== null && 'engines' in manifest ? manifest.engines : null;
const declaredNode =
  typeof engines === 'object' && engines !== null && 'node' in engines && typeof engines.node === 'string'
    ? engines.node
    : fail('package.json declares no engines.node floor');
const [floorMajor, floorMinor] = nodeFloor(declaredNode);
const [nodeMajor, nodeMinor] = process.versions.node.split('.').map(Number);
if (nodeMajor < floorMajor || (nodeMajor === floorMajor && nodeMinor < floorMinor)) {
  fail(`Node ${process.versions.node} is too old for the repo's TypeScript scripts (package.json wants ${declaredNode})`);
}

const os: OsName =
  process.platform === 'darwin' || process.platform === 'linux' || process.platform === 'win32'
    ? process.platform
    : fail(`unsupported host ${process.platform}: set ANDROID_HOME/ANDROID_NDK_HOME/JAVA_HOME yourself`);

const env: NodeJS.ProcessEnv = process.env;
const home = homedir();
const javaBinary = (jdkHome: string): string => join(jdkHome, 'bin', os === 'win32' ? 'java.exe' : 'java');
let brewPrefix = '';
const failures: string[] = [];
const npmBin = resolve('node_modules', '.bin');
let dxToolDirs: string[] = [];

/** Runs one section; a section's failure never cancels its siblings. */
async function section(name: string, body: () => Promise<void>): Promise<void> {
  try {
    await body();
  } catch (error) {
    failures.push(`${name}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

const requireFile = (path: string, hint: string): string => (existsSync(path) ? path : fail(hint));

// ── Android: JDK, SDK packages, NDK ─────────────────────────────────────────

// oxlint-disable-next-line max-lines-per-function -- ordered setup: JDK probe, install and re-probe are one ordered sequence
async function javaHome(): Promise<string> {
  const usable = async (jdkHome: string): Promise<boolean> => {
    if (!existsSync(javaBinary(jdkHome))) return false;
    const version = await run(javaBinary(jdkHome), ['-version'], { quiet: true });
    const major = Number(/version "(\d+)/u.exec(version.stdout + version.stderr)?.[1] ?? 0);
    return major >= JDK_MIN && major <= JDK_MAX;
  };

  brewPrefix = (await hasCommand('brew')) ? (await brewQuery(['--prefix'])).stdout.trim() : '';
  const candidates: string[] = [];
  if (env.JAVA_HOME !== undefined && env.JAVA_HOME !== '') candidates.push(env.JAVA_HOME);
  if (brewPrefix) {
    candidates.push(join(brewPrefix, 'opt', 'openjdk', 'libexec', 'openjdk.jdk', 'Contents', 'Home'));
    try {
      candidates.push(
        ...readdirSync(join(brewPrefix, 'opt'))
          .filter((name) => /^openjdk@\d+$/u.test(name))
          .sort((a, b) => compareVersions(b, a))
          .map((name) => join(brewPrefix, 'opt', name, 'libexec', 'openjdk.jdk', 'Contents', 'Home')),
      );
    } catch {
      /* no JDKs under brew */
    }
  }
  if (os === 'darwin') {
    const registered = (await run('/usr/libexec/java_home', [], { quiet: true })).stdout.trim();
    if (registered) candidates.push(registered);
    // last resort: Studio's JDK
    candidates.push('/Applications/Android Studio.app/Contents/jbr/Contents/Home');
  } else if (os === 'linux') {
    try {
      candidates.push(...readdirSync('/usr/lib/jvm').map((name) => join('/usr/lib/jvm', name)));
    } catch {
      /* no /usr/lib/jvm */
    }
    candidates.push('/opt/android-studio/jbr');
  } else {
    candidates.push(...windowsJdkHomes(env, home), 'C:\\Program Files\\Android\\Android Studio\\jbr');
  }
  for (const candidate of [...new Set(candidates)]) {
    // oxlint-disable-next-line no-await-in-loop -- ordered probe: stop at the first usable JDK, spawning one java at a time
    if (await usable(candidate)) return candidate;
  }

  const install = tagged('java');
  install(`no OpenJDK ${JDK_MIN}-${JDK_MAX} found; installing`);
  if (os === 'win32') {
    if (!(await hasCommand('winget'))) throw new Error(`install Microsoft's OpenJDK: winget install ${WINGET_JDK}`);
    install(`winget install --id ${WINGET_JDK}`);
    const status = await runVisible('winget', ['install', '--id', WINGET_JDK, '--accept-package-agreements', '--accept-source-agreements', '--silent']);
    if (status !== 0) throw new Error(`winget could not install ${WINGET_JDK}`);
  } else {
    // A single JDK: the newest one the ceiling allows. On macOS that is brew's
    // `openjdk@<ceiling>` (an alias for the unversioned keg while the ceiling is the
    // current GA, a real versioned formula afterwards); on Linux the newest in-range
    // apt package. The unversioned "latest" is deliberately not installed too — the
    // probe above already uses it when it happens to be in range.
    const pkg =
      os === 'darwin'
        ? `openjdk@${JDK_MAX}`
        : newest(
            (
              (await run('apt-cache', ['search', '--names-only', '^openjdk-[0-9]+-jdk$'], { quiet: true })).stdout.match(/^openjdk-\d+-jdk$/gmu) ?? []
            ).filter((name) => Number(name.split('-')[1] ?? 0) <= JDK_MAX),
            (name) => name.split('-')[1] ?? '0',
          );
    if (pkg === undefined) throw new Error(`no OpenJDK <= ${JDK_MAX} available; install one that the pinned Gradle accepts`);
    install(`installing ${pkg}`);
    if (os === 'darwin') {
      if (!(await hasCommand('brew'))) throw new Error('install Homebrew (https://brew.sh), then re-run');
      if ((await brewInstall(['install', pkg])) !== 0) throw new Error(`could not install ${pkg}`);
    } else {
      const sudo = process.getuid?.() === 0 ? [] : ['sudo', '-n'];
      if (sudo.length && (await run('sudo', ['-n', 'true'], { quiet: true })).status !== 0) throw new Error(`run: sudo apt-get install -y ${pkg}`);
      if ((await runVisible('apt-get', [...sudo, 'install', '-y', pkg])) !== 0) throw new Error(`apt-get install ${pkg} failed`);
    }
  }
  for (const candidate of [...new Set(candidates)]) {
    // oxlint-disable-next-line no-await-in-loop -- ordered re-probe: stop at the first usable JDK, spawning one java at a time
    if (await usable(candidate)) return candidate;
  }
  throw new Error(`no usable JDK ${JDK_MIN}-${JDK_MAX} after installing; set JAVA_HOME`);
}

// oxlint-disable-next-line max-lines-per-function -- ordered setup: bootstrap, install and assertions are one ordered sequence
async function androidSetup(): Promise<void> {
  const done = tagged('android');
  const javaHomePath = await javaHome();
  const javaVersion = /version "(\d+)/u.exec((await run(javaBinary(javaHomePath), ['-version'], { quiet: true })).stderr)?.[1] ?? '?';
  done(`JDK ${javaVersion} at ${javaHomePath}`);

  const sdk = env.ANDROID_SDK_ROOT ?? env.ANDROID_SDK ?? env.ANDROID_HOME ?? sdkDefault(os, env, home);
  mkdirSync(sdk, { recursive: true });
  done(`SDK root ${sdk}`);
  const cmdlineTools = join(sdk, 'cmdline-tools', 'latest');
  const sdkmanagerClasspath = join(cmdlineTools, 'lib', 'sdkmanager-classpath.jar');

  // Straight from the JVM: no .bat/.sh launcher, no shell, identical on all hosts.
  // --sdk_root is explicit because that launcher is what normally infers it.
  const sdkmanager = (args: string[], input?: string): Promise<RunResult> =>
    run(javaBinary(javaHomePath), ['-classpath', sdkmanagerClasspath, SDKMANAGER_MAIN, `--sdk_root=${sdk}`, ...args], { quiet: true, input });

  if (!existsSync(sdkmanagerClasspath)) {
    const tag = os === 'darwin' ? 'mac' : os === 'linux' ? 'linux' : 'win';
    const index = await fetch('https://dl.google.com/android/repository/repository2-3.xml', { signal: AbortSignal.timeout(60_000) }).catch((error: unknown) => {
      throw new Error(`could not fetch Google's SDK repository index: ${String(error)}`);
    });
    const xml = await index.text();
    const build = newest([...xml.matchAll(new RegExp(`commandlinetools-${tag}-(\\d+)_latest\\.zip`, 'gu'))].map((match) => match[1]), String);
    if (build === undefined) throw new Error(`no commandlinetools-${tag} package in Google's repository index`);
    done(`bootstrapping cmdline-tools (build ${build})`);
    const work = mkdtempSync(join(tmpdir(), 'dx-setup-'));
    const zip = join(work, 'tools.zip');
    const download = await fetch(`https://dl.google.com/android/repository/commandlinetools-${tag}-${build}_latest.zip`, {
      signal: AbortSignal.timeout(900_000),
    });
    writeFileSync(zip, Buffer.from(await download.arrayBuffer()));
    // Windows 10+ ships bsdtar, which reads zip; unzip is the POSIX default.
    const extractor = os === 'win32' || !(await hasCommand('unzip')) ? ['tar', '-xf', zip, '-C', work] : ['unzip', '-q', zip, '-d', work];
    if ((await run(extractor[0], extractor.slice(1))).status !== 0) throw new Error(`could not extract ${zip}`);
    rmSync(cmdlineTools, { recursive: true, force: true });
    cpSync(join(work, 'cmdline-tools'), cmdlineTools, { recursive: true });
    rmSync(work, { recursive: true, force: true });
  }

  const dioxusToml = requireFile('Dioxus.toml', 'Dioxus.toml not found; run this from the repo root');
  const toml = readFileSync(dioxusToml, 'utf8');
  const tomlNumber = (key: string): string => new RegExp(`^ *${key} *= *(\\d+)`, 'mu').exec(toml)?.[1] ?? '';
  const compileSdk = tomlNumber('compile_sdk') || fail('Dioxus.toml [android] compile_sdk not found');
  const minSdk = tomlNumber('min_sdk') || fail('Dioxus.toml [android] min_sdk not found');

  done(`querying packages (compile_sdk ${compileSdk}, min_sdk ${minSdk})`);
  // --list hits Google's repository XML; a flaky fetch must not kill the setup.
  let listed = await sdkmanager(['--list']);
  if (listed.status !== 0) listed = await sdkmanager(['--list']);
  if (listed.status !== 0) throw new Error(`sdkmanager --list failed twice: ${listed.stderr}`);

  const pick = (pattern: RegExp, label: string): string =>
    newest(listed.stdout.match(pattern) ?? [], String) ?? fail(`no ${label} offered by sdkmanager (run 'sdkmanager --list')`);
  // Newest of each, plus the set dx compiles against (Dioxus.toml's compile SDK).
  const platforms = [...new Set([pick(/platforms;android-[0-9]+(?:\.[0-9]+)?/gu, 'platforms;android-*'), pick(new RegExp(`platforms;android-${compileSdk}\\.[0-9]+`, 'gu'), 'compile platform')])];
  const buildTools = [...new Set([pick(/build-tools;[0-9]+(?:\.[0-9]+){1,2}/gu, 'build-tools;*'), pick(new RegExp(`build-tools;${compileSdk}\\.[0-9]+\\.[0-9]+`, 'gu'), 'compile build-tools')])];
  const ndkPackage = pick(/ndk;[0-9]+(?:\.[0-9]+)+/gu, 'ndk');
  const packages = ['platform-tools', ...platforms, ...buildTools, ndkPackage];
  done(`installing ${packages.join(' + ')} (idempotent; older versions stay)`);
  // the licenses dir is what AGP reads
  await sdkmanager(['--licenses'], 'y\n'.repeat(50));
  const install = await sdkmanager(packages);
  if (install.status !== 0) throw new Error(`package install failed: ${install.stderr}`);

  // Assert exactly what dx will resolve. dx takes the lexicographically newest
  // NDK dir, not the newest version.
  const ndkDir = join(sdk, 'ndk', readdirSync(join(sdk, 'ndk')).sort().at(-1) ?? fail(`no NDK installed under ${join(sdk, 'ndk')}`));
  const linker = join(ndkDir, 'toolchains', 'llvm', 'prebuilt', prebuiltDir(os), 'bin', `aarch64-linux-android${minSdk}-clang${os === 'win32' ? '.cmd' : ''}`);
  if (!existsSync(linker)) {
    throw new Error(
      `dx would fail here: ${linker} is missing.
  Newest installed NDK is ${ndkDir.split(/[\\/]/u).at(-1)}, which lacks API ${minSdk} clang wrappers.
  Repair: sdkmanager --install "${ndkPackage}"   (or delete the stale dir: ${ndkDir})`,
    );
  }
  for (const pkg of [...platforms, ...buildTools]) {
    const parent = join(sdk, pkg.slice(0, pkg.indexOf(';')));
    if (!existsSync(join(parent, pkg.slice(pkg.indexOf(';') + 1)))) throw new Error(`${pkg} missing under ${parent}`);
  }
  // dx compiles against the newest installed minor of the compile SDK's major.
  if (!readdirSync(join(sdk, 'platforms')).some((dir) => dir.startsWith(`android-${compileSdk}.`))) throw new Error(`no android-${compileSdk}.x platform installed`);
  if (!existsSync(join(sdk, 'platform-tools', os === 'win32' ? 'adb.exe' : 'adb'))) throw new Error(`adb missing under ${join(sdk, 'platform-tools')}`);

  done(`OK — platforms/${platforms.map((p) => p.slice(p.indexOf(';') + 1)).join(' + ')}, build-tools/${buildTools.map((p) => p.slice(p.indexOf(';') + 1)).join(' + ')}, ndk/${ndkDir.split(/[\\/]/u).at(-1)} (API ${minSdk} clang present)`);
  android = { sdk, ndkDir, javaHome: javaHomePath };
}

let android: { sdk: string; ndkDir: string; javaHome: string } | undefined;

// ── Rust: components and targets the repo's builds need ─────────────────────

async function rustSetup(): Promise<void> {
  const done = tagged('rust');
  if (!(await hasCommand('rustup'))) {
    throw new Error('rustup not found — install Rust from https://rustup.rs, then re-run');
  }
  // CI runs clippy/fmt; build-std needs rust-src
  const components = ['clippy', 'rustfmt', 'rust-src'];
  const targets = ['wasm32-unknown-unknown', 'aarch64-linux-android', 'aarch64-apple-ios'];
  if (os === 'darwin') targets.push('aarch64-apple-ios-sim', 'x86_64-apple-ios');
  const results = await Promise.all([
    ...components.map((component) => run('rustup', ['component', 'add', component], { quiet: true })),
    ...targets.map((target) => run('rustup', ['target', 'add', target], { quiet: true })),
  ]);
  const failed = [...components, ...targets].filter((_, index) => results[index].status !== 0);
  if (failed.length) throw new Error(`rustup could not add: ${failed.join(', ')}`);
  done(`components ${components.join(', ')}; targets ${targets.join(', ')}`);
}

// ── dx tooling: NO_DOWNLOADS discovery and CLI/app revision coupling ────────

// The locked CLI's versions: wasm-bindgen comes from this repo's Cargo.lock,
// binaryen and esbuild are pinned by packages/dx/src/{wasm_opt,esbuild}.rs.
const BINARYEN_VERSION = '129';
const ESBUILD_VERSION = '0.27.3';
const wasmBindgenVersion = (): string => {
  const lockfile = readFileSync('Cargo.lock', 'utf8');
  return /name = "wasm-bindgen"\r?\nversion = "([^"]+)"/u.exec(lockfile)?.[1] ?? fail('Cargo.lock does not declare wasm-bindgen');
};

async function dxTooling(): Promise<void> {
  const done = tagged('dx');
  const cli = join(home, '.cargo', 'bin', os === 'win32' ? 'dx.exe' : 'dx');
  if (!existsSync(cli)) {
    done(`warn: ${cli} is missing — install the CLI from the revision in Cargo.lock, then re-run`);
    return;
  }

  // The app and the CLI share one Dioxus revision; a mismatch is the usual cause
  // of odd build errors after a pull.
  const locked = /dioxus\.git\?branch=main#([0-9a-f]+)/u.exec(readFileSync('Cargo.lock', 'utf8'))?.[1];
  const reported = /\(([0-9a-f]{7,})\)/u.exec((await run(cli, ['--version'], { quiet: true })).stdout)?.[1];
  if (locked !== undefined && reported !== undefined && !locked.startsWith(reported)) {
    done(`warn: dx is ${reported} but the app builds against ${locked.slice(0, 7)} — reinstall the CLI at that revision`);
  } else if (reported !== undefined) {
    done(`CLI ${reported} matches Cargo.lock`);
  }

  // dx resolves Tailwind through which() under NO_DOWNLOADS=1. The npm package is
  // the normal source; TAILWIND_BIN still points to a standalone native binary
  // (Windows requires the .exe because npm's shim is not one).
  const tools = join(dxHome(os, env, home), 'tools');
  const bindgen = join(tools, `wasm-bindgen-${wasmBindgenVersion()}`, executableName(os, 'wasm-bindgen'));
  const wasmOpt = join(tools, `binaryen-${BINARYEN_VERSION}`, 'bin', executableName(os, 'wasm-opt'));
  const esbuild = join(tools, `esbuild-${ESBUILD_VERSION}`, executableName(os, 'esbuild'));
  const tailwind = tailwindBinary();
  assertNoDownloadsTools([bindgen, wasmOpt, esbuild, tailwind.bin]);

  const expected = wasmBindgenVersion();
  const bindgenReport = /wasm-bindgen\s+(\S+)/u.exec((await run(bindgen, ['--version'], { quiet: true })).stdout)?.[1];
  if (bindgenReport !== expected) {
    throw new Error(`managed wasm-bindgen is ${bindgenReport ?? 'unreadable'} at ${bindgen}; Cargo.lock requires ${expected}`);
  }

  dxToolDirs = [tailwind.dir, npmBin, join(tools, `wasm-bindgen-${expected}`), join(tools, `binaryen-${BINARYEN_VERSION}`), join(tools, `esbuild-${ESBUILD_VERSION}`)].filter(
    (dir, index, dirs) => dirs.indexOf(dir) === index,
  );
  done(`NO_DOWNLOADS ready: tailwind ${env.TAILWIND_BIN !== undefined && env.TAILWIND_BIN !== '' ? tailwind.bin : 'npm'}, wasm-bindgen ${expected}, binaryen ${BINARYEN_VERSION}, esbuild ${ESBUILD_VERSION}`);
}

/** The Tailwind binary dx will `which()` under NO_DOWNLOADS: an explicit
 *  `TAILWIND_BIN` override (must be a native executable) or the npm CLI. */
function tailwindBinary(): { bin: string; dir: string } {
  const override = env.TAILWIND_BIN;
  if (override === undefined || override === '') return { bin: join(npmBin, os === 'win32' ? 'tailwindcss.cmd' : 'tailwindcss'), dir: npmBin };
  const bin = resolve(override);
  if (!existsSync(bin)) throw new Error(`TAILWIND_BIN names a missing file: ${bin}`);
  if (os === 'win32' && !bin.toLowerCase().endsWith('.exe')) {
    throw new Error(`TAILWIND_BIN must name a native Windows .exe (npm's shim is not one): ${bin}`);
  }
  return { bin, dir: dirname(bin) };
}

/** Every tool dx resolves via which() under NO_DOWNLOADS must exist. */
function assertNoDownloadsTools(paths: string[]): void {
  const missing = paths.filter((path) => !existsSync(path));
  if (missing.length === 0) return;
  throw new Error(
    `NO_DOWNLOADS=1 requires these tools, but they are missing:\n  ${missing.join('\n  ')}\n` +
      'Repair: run npm ci for the npm Tailwind CLI and install the locked dx once with downloads enabled so it creates its managed tools directory.',
  );
}

// ── Platform extras: Xcode/xtool (macOS), WebView2 (Windows), libs (Linux) ──

// oxlint-disable-next-line max-lines-per-function -- one warn-or-install branch per host platform, kept in one place
async function platformExtras(): Promise<void> {
  const done = tagged('platform');
  if (os === 'darwin') {
    if ((await run('xcode-select', ['-p'], { quiet: true })).status !== 0) {
      done('warn: no Xcode selected — install Xcode from the App Store, then `sudo xcode-select -s /Applications/Xcode.app`');
    }
    if (await hasCommand('brew')) {
      const upgrading = await hasCommand('xtool');
      done(`${upgrading ? 'updating' : 'installing'} xtool (signs + installs the iOS device build)`);
      // upstream moved the formula into its own tap; plain `brew install xtool` fails
      if ((await brewInstall([upgrading ? 'upgrade' : 'install', 'xtool-org/tap/xtool'])) !== 0) {
        done('warn: xtool install/upgrade failed; install it manually before deploying to a device');
      }
    }
    if (await hasCommand('xtool')) {
      if (process.stdin.isTTY && (await confirm('Run "xtool auth login" now (Apple ID; free personal team is fine)?', false))) {
        await runVisible('xtool', ['auth', 'login']);
      } else {
        done("run 'xtool auth login' once before deploying to a device");
      }
    }
    return;
  }

  if (os === 'win32') {
    if (await run('winget', ['list', '--id', WINGET_WEBVIEW, '--exact'], { quiet: true }).then((r) => r.status === 0 && r.stdout.includes(WINGET_WEBVIEW))) {
      done('WebView2 runtime present');
    } else if (await hasCommand('winget')) {
      done(`installing ${WINGET_WEBVIEW} (desktop/webview runtime)`);
      if ((await runVisible('winget', ['install', '--id', WINGET_WEBVIEW, '--accept-package-agreements', '--accept-source-agreements', '--silent'])) !== 0) {
        done(`warn: winget could not install ${WINGET_WEBVIEW}; install the Evergreen runtime manually`);
      }
    } else {
      done(`warn: install ${WINGET_WEBVIEW} manually (winget unavailable)`);
    }
    return;
  }

  // Linux desktop: dx bundles webkit2gtk etc. at link time for `dx build --platform linux`.
  const missing = (await run('pkg-config', ['--exists', ...LINUX_DESKTOP_LIBS], { quiet: true })).status !== 0;
  if (!missing) {
    done('desktop libraries present');
    return;
  }
  if (!(await hasCommand('apt-get'))) {
    done(`warn: install the desktop build libraries: ${APT_DESKTOP_PACKAGES}`);
    return;
  }
  done(`desktop libraries missing; installing ${APT_DESKTOP_PACKAGES}`);
  if (!process.stdin.isTTY && !ASSUME_YES) {
    done(`run: sudo apt-get install -y ${APT_DESKTOP_PACKAGES}`);
    return;
  }
  if (!(await confirm(`Install the desktop build libraries now (sudo apt-get)?`))) {
    done(`skipped; run: sudo apt-get install -y ${APT_DESKTOP_PACKAGES}`);
    return;
  }
  const sudo = process.getuid?.() === 0 ? [] : ['sudo'];
  await runVisible('apt-get', [...sudo, 'install', '-y', ...APT_DESKTOP_PACKAGES.split(' ')]);
}

// ── Run the independent sections concurrently ──────────────────────────────

const started = Date.now();
await Promise.all([
  section('android', androidSetup),
  section('rust', rustSetup),
  section('dx', dxTooling),
  section('platform', platformExtras),
]);
log(`sections finished in ${((Date.now() - started) / 1000).toFixed(1)}s`);
if (failures.length) fail(failures.join('\n  '));
const { sdk, ndkDir, javaHome: jdkHome } = android ?? fail('android setup produced no result');

// ── Report + shell configuration ────────────────────────────────────────────

const envFile = join(env.XDG_CONFIG_HOME ?? join(home, '.config'), 'po', 'env.sh');
const blocks = { start: '# >>> po native toolchain (scripts/dx-setup.ts) >>>', end: '# <<< po native toolchain <<<' };
const envBody = os === 'win32' ? windowsEnv() : posixEnv();

process.stdout.write(`${envBody}\n`);
await configureShells();

/** `$HOME/…` and `$po_brew/…` instead of absolute paths, so the file survives a
 *  moved home, a different brew prefix, or a re-installed toolchain. */
function shorten(path: string): string {
  if (path === home || path.startsWith(`${home}/`)) return `$HOME${path.slice(home.length)}`;
  if (brewPrefix && path.startsWith(`${brewPrefix}/`)) return `$po_brew${path.slice(brewPrefix.length)}`;
  return path;
}

/**
 * The exports every shell sources. Nothing here is version-pinned: the NDK is the
 * newest dir under $ANDROID_HOME/ndk (dx's own rule) and the JDK is the first
 * existing candidate in this script's order, so a toolchain update is picked up by
 * the next shell rather than by re-running this script.
 */
function posixEnv(): string {
  // Only in-range JDK candidates may go in: the shell can test existence but not
  // the major, so a newer brew keg would win and Gradle would refuse it. The
  // @MIN..@MAX kegs are in range by construction and probed at shell start.
  const range = brewPrefix
    ? Array.from({ length: JDK_MAX - JDK_MIN + 1 }, (_, i) => JDK_MAX - i).map((major) => join(brewPrefix, 'opt', `openjdk@${major}`, 'libexec', 'openjdk.jdk', 'Contents', 'Home'))
    : [];
  const extras = [
    ...(brewPrefix ? [join(brewPrefix, 'opt', 'openjdk', 'libexec', 'openjdk.jdk', 'Contents', 'Home')] : []),
    ...(os === 'darwin' ? ['/Applications/Android Studio.app/Contents/jbr/Contents/Home'] : ['/opt/android-studio/jbr']),
  ];
  const override = env.JAVA_HOME !== undefined && env.JAVA_HOME !== '' ? [env.JAVA_HOME] : [];
  const jdkHomes = [...new Set([...override, ...range, ...extras])].map(shorten);
  return [
    '# Generated by scripts/dx-setup.ts — re-run it after toolchain updates.',
    `po_brew="\${HOMEBREW_PREFIX:-${brewPrefix || '/opt/homebrew'}}"`,
    'po_dx=""',
    // The versioned dx tool dirs are refreshed by re-running this script after a dx update.
    ...dxToolDirs.map((dir) => `po_dx="\${po_dx:+\${po_dx}:}${shorten(dir)}"`),
    `export ANDROID_HOME="${shorten(sdk)}"`,
    'export ANDROID_SDK_ROOT="$ANDROID_HOME"',
    'po_ndk="$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | LC_ALL=C sort | tail -1)"',
    '[ -n "$po_ndk" ] && export ANDROID_NDK_HOME="$po_ndk" NDK_HOME="$po_ndk"',
    'po_jdk=""',
    `for po_c in ${jdkHomes.map((c) => `"${c}"`).join(' ')}; do`,
    '  if [ -x "$po_c/bin/java" ]; then po_jdk="$po_c"; break; fi',
    'done',
    '[ -n "$po_jdk" ] && export JAVA_HOME="$po_jdk"',
    // The locked dx uses which() for Tailwind and the wasm tools instead of downloading.
    'export NO_DOWNLOADS=1',
    // macOS /etc/zprofile runs path_helper, which pushes PATH entries from
    // .zshenv to the back; .zprofile sources this file again, so the test is
    // "are our dirs already first", not "are they anywhere in PATH".
    'case "$PATH" in',
    `  "${shorten(join(sdk, 'cmdline-tools', 'latest', 'bin'))}:"*) ;;`,
    '  *) export PATH="${po_dx:+$po_dx:}:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator${JAVA_HOME:+:$JAVA_HOME/bin}:$PATH" ;;',
    'esac',
    'unset po_brew po_ndk po_jdk po_c po_dx',
  ].join('\n');
}

/** Windows keeps registry values (`setx` is absolute by nature), but the SDK
 *  stays `%LOCALAPPDATA%`-relative and the NDK is resolved the way dx does. */
function windowsEnv(): string {
  const sdkExpr = sdk === sdkDefault('win32', env, home) ? '$env:LOCALAPPDATA\\Android\\Sdk' : sdk;
  return [
    `$env:ANDROID_HOME = "${sdkExpr}"`,
    '$env:ANDROID_SDK_ROOT = $env:ANDROID_HOME',
    '$env:ANDROID_NDK_HOME = (Get-ChildItem "$env:ANDROID_HOME\\ndk" -Directory | Sort-Object -Property Name | Select-Object -Last 1).FullName',
    '$env:NDK_HOME = $env:ANDROID_NDK_HOME',
    `$env:JAVA_HOME = "${jdkHome}"`,
    // The locked dx uses which() for Tailwind and the wasm tools instead of downloading.
    '$env:NO_DOWNLOADS = "1"',
    `$env:Path = "${[...dxToolDirs, join(jdkHome, 'bin')].join(';')};$env:ANDROID_HOME\\cmdline-tools\\latest\\bin;$env:ANDROID_HOME\\platform-tools;$env:ANDROID_HOME\\emulator;" + $env:Path`,
  ].join('\n');
}

/** Drops our block, whichever script name wrote it. */
function stripBlock(text: string): string {
  const kept: string[] = [];
  let skipping = false;
  for (const line of text.split('\n')) {
    if (line.startsWith('# >>> po native toolchain')) skipping = true;
    else if (line.startsWith('# <<< po native toolchain')) skipping = false;
    else if (!skipping) kept.push(line);
  }
  return kept.join('\n');
}

interface StartupScan {
  exists: boolean;
  hasBlock: boolean;
  /** Values these files already set, which our block will override. */
  overrides: string[];
}

function scanStartupFile(rc: string): StartupScan {
  if (!existsSync(rc)) return { exists: false, hasBlock: false, overrides: [] };
  const text = readFileSync(rc, 'utf8');
  const overrides: string[] = [];
  let inBlock = false;
  for (const line of text.split('\n')) {
    if (line.startsWith('# >>> po native toolchain')) inBlock = true;
    else if (line.startsWith('# <<< po native toolchain')) inBlock = false;
    else if (!inBlock) {
      const match = /^\s*export\s+(JAVA_HOME|ANDROID_HOME|ANDROID_SDK_ROOT|ANDROID_NDK_HOME|NDK_HOME)=(.+)$/u.exec(line);
      if (match) overrides.push(`${match[1]}=${match[2].replace(/^["']|["']$/gu, '')}`);
    }
  }
  return { exists: true, hasBlock: /^# >>> po native toolchain/mu.test(text), overrides };
}

/**
 * Which startup files to source from. zsh needs `.zshenv` (runs for every zsh,
 * including non-interactive) *and* `.zprofile`: macOS /etc/zprofile runs
 * path_helper, which demotes PATH entries added earlier. bash needs `.bashrc` for
 * interactive shells and `.bash_profile` for login ones (unless that already
 * sources `.bashrc`). Everything else gets `.profile`.
 */
function startupFiles(): string[] {
  const zdot = process.env.ZDOTDIR ?? home;
  const shell = (process.env.SHELL ?? '').split('/').at(-1) ?? '';
  const files: string[] = [];
  if (!shell || shell === 'zsh' || existsSync(join(zdot, '.zshrc'))) {
    files.push(join(zdot, '.zshenv'), join(zdot, '.zprofile'));
  }
  if (shell === 'bash' || existsSync(join(home, '.bashrc'))) {
    files.push(join(home, '.bashrc'));
    const bashProfile = join(home, '.bash_profile');
    if (existsSync(bashProfile) && !readFileSync(bashProfile, 'utf8').includes('.bashrc')) files.push(bashProfile);
  }
  if (files.length === 0 || shell === 'sh') files.push(join(home, '.profile'));
  return [...new Set(files)];
}

async function configureShells(): Promise<void> {
  if (os === 'win32') return configureWindowsEnv();
  const targets = startupFiles();
  // empty SHELL is as good as unset for picking the shell to probe
  const shell = process.env.SHELL ?? '';
  const fresh = (await run(shell === '' ? '/bin/bash' : shell, ['-lic', 'printf %s "$JAVA_HOME"'], { quiet: true })).stdout.trim();
  if (fresh === jdkHome && targets.every((rc) => scanStartupFile(rc).hasBlock) && existsSync(envFile)) {
    log(`shells already configured: JAVA_HOME=${fresh} via ${envFile}`);
    return;
  }
  log(fresh ? `new shells currently resolve JAVA_HOME=${fresh}` : 'new shells currently have no JAVA_HOME');
  if (!process.stdin.isTTY && !ASSUME_YES) {
    log(`non-interactive run: eval the lines above, or re-run with --yes to write ${envFile} + ${targets.join(', ')}`);
    return;
  }
  if (!(await confirm(`Point JAVA_HOME and java at the newest in-range JDK via ${envFile}?`))) {
    log('skipped; paste the exports above or re-run to try again');
    return;
  }

  mkdirSync(join(envFile, '..'), { recursive: true });
  writeFileSync(envFile, `${envBody}\n`);
  log(`wrote ${envFile} (versions resolve at shell start)`);
  for (const rc of targets) {
    const scan = scanStartupFile(rc);
    const base = stripBlock(scan.exists ? readFileSync(rc, 'utf8') : '');
    const lead = base === '' || base.endsWith('\n') ? base : `${base}\n`;
    writeFileSync(rc, `${lead}${blocks.start}\n. "${envFile}"\n${blocks.end}\n`);
    log(`${scan.exists ? 'sourced' : 'created'} ${rc}${scan.overrides.length ? ` — overrides ${scan.overrides.join(', ')}` : ''}`);
  }
  log(`system-wide, if you want it: sudo update-alternatives --install /usr/bin/java java ${join(jdkHome, 'bin')}/java 2000`);
  if (jdkHome.includes('/opt/') && jdkHome.includes('openjdk')) {
    const keg = jdkHome.split('/libexec/')[0];
    const link = join('/Library/Java/JavaVirtualMachines', `${keg.split('/').at(-1)?.replace('@', '-')}.jdk`);
    log(`for /usr/libexec/java_home and GUI apps: sudo ln -sfn ${keg}/libexec/openjdk.jdk ${link}`);
    if (await confirm('Run that symlink now (asks for your password)?')) await runVisible('sudo', ['ln', '-sfn', `${keg}/libexec/openjdk.jdk`, link]);
  }
}

async function configureWindowsEnv(): Promise<void> {
  const query = async (name: string): Promise<string> =>
    (await run('reg', ['query', 'HKCU\\Environment', '/v', name], { quiet: true })).stdout.match(new RegExp(`${name}\\s+REG_[A-Z_]+\\s+(.*)`, 'u'))?.[1].trim() ?? '';
  const persisted = await query('JAVA_HOME');
  if (persisted === jdkHome) {
    log(`registry already configured: user JAVA_HOME=${persisted}`);
    return;
  }
  log(`user JAVA_HOME is ${persisted || 'unset'}`);
  if (!process.stdin.isTTY && !ASSUME_YES) {
    log('non-interactive run: paste the lines above into this shell, or re-run with --yes to persist them');
    return;
  }
  if (!(await confirm('Point JAVA_HOME and the Android vars at this toolchain for new shells?'))) {
    log('skipped; paste the lines above or re-run to try again');
    return;
  }
  for (const [key, value] of Object.entries({ JAVA_HOME: jdkHome, ANDROID_HOME: sdk, ANDROID_SDK_ROOT: sdk, ANDROID_NDK_HOME: ndkDir, NDK_HOME: ndkDir, NO_DOWNLOADS: '1' })) {
    // oxlint-disable-next-line no-await-in-loop -- ordered registry writes, each with its own failure log
    if ((await run('setx', [key, value], { quiet: true })).status !== 0) log(`warn: setx ${key} failed`);
  }
  // setx truncates at 1024 chars, so PATH is extended through the user registry.
  const prepend = [...dxToolDirs, join(jdkHome, 'bin'), join(sdk, 'cmdline-tools', 'latest', 'bin'), join(sdk, 'platform-tools'), join(sdk, 'emulator')].join(';');
  const script =
    `$p = [Environment]::GetEnvironmentVariable('Path','User'); ` +
    `[Environment]::SetEnvironmentVariable('Path', '${prepend};' + $p, 'User')`;
  if ((await run('powershell', ['-NoProfile', '-Command', script])).status === 0) {
    log(`set the Android vars for new shells and prepended the user PATH with ${prepend}`);
  } else {
    log(`warn: could not extend the user PATH; add ${prepend} to Path manually`);
  }
}

/** One line of user input via readline, so it works for TTYs and pipes alike. */
async function confirm(question: string, allowAssumeYes = true): Promise<boolean> {
  if (allowAssumeYes && ASSUME_YES) {
    log(`${question} -> yes (--yes)`);
    return true;
  }
  if (!process.stdin.isTTY) return false;
  const rl = createInterface({ input: process.stdin, output: process.stderr });
  try {
    return /^\s*y/iu.test(await rl.question(`${question} [y/N] `));
  } catch {
    // stdin closed (Ctrl-D)
    return false;
  } finally {
    rl.close();
  }
}
