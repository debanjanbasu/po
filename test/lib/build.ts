// Build-artifact freshness for the tandem harness.
//
// Every surface used to build only when its artifact was *missing*, so a
// stale bundle from a previous day kept running while the source moved on.
// That silently invalidated runs: a web peer kept serving a build from the
// old `dx`/veilid revision, so a source fix never reached it.
//
// The check is by mtime against the build inputs, which is cheap and needs no
// manifest format knowledge. A build older than the newest build input is
// stale and must be rebuilt.

import { readdirSync, statSync, type Dirent, type Stats } from 'node:fs';
import { join } from 'node:path';

/**
 * Cargo feature that compiles the app's `e2e` seams (`src/config.rs`) into any
 * profile. Off by default — a distributed build never enables it, and a build
 * without it and without debug assertions has no seams at all.
 */
export const E2E_SEAM_FEATURE = 'e2e-seams';

/** Directories whose contents are build output, not source. */
const IGNORED_DIRS: Record<string, true> = {
	'.git': true,
	'.github': true,
	'node_modules': true,
	'target': true,
	'dist': true,
};

/** Extensions that cannot affect the compiled app. */
const IGNORED_EXT: Record<string, true> = {
	'.md': true,
	'.txt': true,
	'.png': true,
	'.jpg': true,
	'.jpeg': true,
	'.gif': true,
	'.svg': true,
	'.ico': true,
	'.woff': true,
	'.woff2': true,
};

/**
 * Directories that can change the compiled artifact. An allowlist, not a
 * denylist: a full-tree walk counted the harness's own output as source, so
 * every run wrote `test/runs/**` newer than the artifact it had just built and
 * the next run rebuilt all over again — four minutes for web, eleven for
 * ios-sim. npm manifests and the Astro site did the same. Neither changes a
 * `dx` build, so neither belongs here.
 */
const SOURCE_DIRS: readonly string[] = ['src', 'assets', 'native'];

/**
 * Files that can change the compiled artifact and are not under a
 * `SOURCE_DIRS` entry. The last two are inputs `build.rs` declares itself
 * (`cargo:rerun-if-changed`): the embedded `about.json` and the `.env` the app
 * pepper is read from — without them an edit to either leaves the artifact
 * looking fresh, and the harness runs the previous build.
 */
const SOURCE_FILES: readonly string[] = [
	'build.rs',
	'Cargo.toml',
	'Cargo.lock',
	'Dioxus.toml',
	'rust-toolchain.toml',
	'.cargo/config.toml',
	'shared/about.json',
	'.env',
];

/** Newest mtime (ms) across the build inputs, or 0 when none can be read. */
export function newestSourceMtime(root = process.cwd()): number {
	let newest = 0;
	const note = (path: string): void => {
		try {
			const mtime = statSync(path).mtimeMs;
			if (mtime > newest) newest = mtime;
		} catch {
			// A file that vanished mid-walk cannot make the build stale.
		}
	};
	const walk = (dir: string): void => {
		let entries: Dirent[];
		try {
			entries = readdirSync(dir, { withFileTypes: true });
		} catch {
			return;
		}
		for (const entry of entries) {
			// Dot-entries cover Gradle's `native/android/**/.gradle/` output.
			if (entry.name.startsWith('.')) continue;
			const path = join(dir, entry.name);
			if (entry.isDirectory()) {
				if (!IGNORED_DIRS[entry.name]) walk(path);
				continue;
			}
			if (!entry.isFile()) continue;
			const dot = entry.name.lastIndexOf('.');
			if (dot >= 0 && IGNORED_EXT[entry.name.slice(dot)]) continue;
			note(path);
		}
	};
	for (const dir of SOURCE_DIRS) walk(join(root, dir));
	for (const file of SOURCE_FILES) note(join(root, file));
	return newest;
}

/** Mtime (ms) of the artifact, or 0 when it does not exist. */
export function artifactMtime(path: string): number {
	let st: Stats;
	try {
		st = statSync(path);
	} catch {
		return 0;
	}
	if (!st.isDirectory()) return st.mtimeMs;
	// A bundle directory's own mtime only moves when an entry is added or
	// removed, so a rebuild in place leaves it at the day the bundle was first
	// written — the macOS and iOS bundles here read Nov-2 while their binaries
	// were rebuilt today, and `isStale` then rebuilt on every single run. Take
	// the newest file inside instead.
	let newest = st.mtimeMs;
	const walk = (dir: string): void => {
		let entries: Dirent[];
		try {
			entries = readdirSync(dir, { withFileTypes: true });
		} catch {
			return;
		}
		for (const entry of entries) {
			const child = join(dir, entry.name);
			if (entry.isDirectory()) {
				walk(child);
				continue;
			}
			if (!entry.isFile()) continue;
			try {
				const mtime = statSync(child).mtimeMs;
				if (mtime > newest) newest = mtime;
			} catch {
				// Vanished mid-walk.
			}
		}
	};
	walk(path);
	return newest;
}

/**
 * True when the artifact is missing or older than the newest source file.
 *
 * `artifact` may be a file or a directory — dx writes a bundle directory for
 * native platforms and a `public/` tree for web.
 */
export function isStale(artifact: string, root = process.cwd()): boolean {
	const built = artifactMtime(artifact);
	if (built === 0) return true;
	return built < newestSourceMtime(root);
}

/**
 * Build `artifact` when it is missing or stale, then assert it exists — the
 * sequence every native surface needs, with only the middle step differing.
 * `build(missing)` does the platform work (dx invocation, bundle normalization).
 */
export async function ensureBuilt(opts: {
	artifact: string;
	what: string;
	note(message: string): void;
	build(missing: boolean): Promise<void>;
	/** Appended to the "no artifact after build" error, for platform hints. */
	hint?: string;
}): Promise<void> {
	if (!isStale(opts.artifact)) return;
	const missing = artifactMtime(opts.artifact) === 0;
	opts.note(missing ? `${opts.what} missing — building` : `${opts.what} stale — rebuilding`);
	await opts.build(missing);
	if (artifactMtime(opts.artifact) === 0) {
		throw new Error(
			`${opts.what}: no artifact at ${opts.artifact} after build${opts.hint !== undefined && opts.hint !== '' ? ` (${opts.hint})` : ''}`,
		);
	}
}
