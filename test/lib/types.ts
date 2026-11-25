// Shared types for the tandem harness. Type-only; erasable TS (no enums/namespaces).

export type Dict = Record<string, string>;

/** What a surface can actually do. Scenarios gate their steps on this. */
export interface Capabilities {
  /** Can inject environment variables at launch (`PO_E2E*`, `RUST_LOG`, …). */
  env: boolean;
  /** Can deliver an `po://` deep link to the running app. */
  deepLink: boolean;
}

/**
 * One thing a scenario waits for, see docs/device-testing.md § "Observe".
 * `marker` is the `event=<name>` line on the `e2e` target.
 */
export interface Signal {
  label: string;
  marker: string;
}

export interface SignalHit {
  kind: 'marker';
  label: string;
  line: string;
  fields: Dict;
  ms: number;
}

export interface PairingFormValues {
  payload?: string;
  pin?: string;
  password?: string;
}

/**
 * One step of a raw DOM flow, for features that have no purpose-built surface
 * method. Kept as a small op list rather than a per-feature method so a new
 * scenario can drive its own screen without editing the shared surface files.
 *
 * `readText`/`readCount` collect into the returned record under their key.
 */
export type DomOp =
  | { click: string }
  | { fill: [selector: string, value: string] }
  /** Wait for a selector to be visible; the number is a timeout in ms. */
  | { waitFor: [selector: string, timeoutMs: number] }
  | { readText: [key: string, selector: string] }
  | { readCount: [key: string, selector: string] }
  /**
   * Reload the document. A fresh page is the only way to prove that state
   * *persisted* rather than merely stayed in memory — used by S13 for the
   * theme and for the post-logout session check.
   */
  | { reload: true }
  /**
   * Click `selector` and capture the resulting browser download, saving it
   * under the run's evidence dir. The saved path is returned under `key`.
   * Needed to test a real file round trip (S12's backup export).
   */
  | { download: [key: string, selector: string] }
  /** Point a file input at a path on disk. The other half of a file round trip. */
  | { setFiles: [selector: string, path: string] };

export interface Surface {
  readonly name: string;
  readonly caps: Capabilities;
  /** Harness peer ids this surface instance hosts ("a", "b"). */
  readonly peerIds: readonly string[];
  /**
   * What this surface is actually running: resolved artifact paths, device
   * ids, whether an install was skipped. Lands in the run's `result.json`.
   */
  readonly build: Dict;
  setup(): Promise<void>;
  launch(peer: string, env: Dict): Promise<void>;
  /**
   * Restart a peer with a different environment. Native joins need the
   * pairing PIN/password at launch time, so the harness launches them only
   * after the anchor has produced the share.
   */
  relaunch?(peer: string, env: Dict): Promise<void>;
  /** Assert the app reached its first screen; returns the evidence string. */
  ready(peer: string): Promise<string>;
  openUrl(peer: string, url: string): Promise<void>;
  /** Create/sign in an account and reach the app shell. Optional by surface. */
  signIn?(peer: string, username: string, password: string, timeoutMs: number): Promise<void>;
  /** Open the share screen and enter the account password. Optional by surface. */
  openShareScreen?(peer: string, password: string, timeoutMs: number): Promise<void>;
  /** Read the generated share (PIN + deep link) from the share screen. */
  readShare?(peer: string, timeoutMs: number): Promise<{ pin: string; deepLink: string }>;
  /** Open the pairing form on the joiner with payload + PIN + password. */
  openPair?(peer: string, payloadUrl: string, pin: string, password: string, timeoutMs: number): Promise<void>;
  /** Wait until the pairing sync reaches its Complete stage. */
  waitComplete?(peer: string, timeoutMs: number): Promise<void>;
  /** The peer's own account identity, for building a contact deep link. */
  identity?(peer: string): Promise<{ username: string; dhtKey: string }>;
  /** Deliver a contact link/payload to a peer through the app's own paste path. */
  addContact?(peer: string, link: string, timeoutMs: number): Promise<void>;
  /** Accept the pending conversation invite shown in the chat list. */
  acceptInvite?(peer: string, timeoutMs: number): Promise<void>;
  /** Open a conversation in the conversation list by its title. */
  openConversation?(peer: string, title: string, timeoutMs: number): Promise<void>;
  /** Type and send a message in the currently open conversation. */
  sendMessage?(peer: string, text: string, timeoutMs: number): Promise<void>;
  /** Read the message texts currently rendered in the open conversation. */
  readMessages?(peer: string): Promise<string[]>;
  /** Edit the profile DOB in Account settings. */
  editProfile?(peer: string, dob: string, timeoutMs: number): Promise<void>;
  /** Set the profile DOB field and leave it unsaved (no confirm, no save). */
  stageProfileEdit?(peer: string, dob: string, timeoutMs: number): Promise<void>;
  /** The profile's DOB-conflict row text, or null when it is not shown. */
  readProfileConflict?(peer: string): Promise<string | null>;
  /** The peer's visible page text, whitespace-collapsed. */
  readText?(peer: string): Promise<string>;
  /** Navigate to the Pairing route without generating a share. */
  openPairing?(peer: string, password: string, timeoutMs: number): Promise<void>;
  /**
   * Run a raw DOM flow (see [`DomOp`]). The generic escape hatch for screens
   * with no purpose-built method; surfaces without a DOM leave it undefined
   * and scenarios gate on it.
   */
  dom?(peer: string, ops: DomOp[]): Promise<Record<string, string | number>>;
  /** Force-stop the peer and relaunch it with `env`. */
  restart?(peer: string, env: Dict): Promise<void>;
  /**
   * Read the app's rendered DOM: what `selector` matched, or null when
   * nothing did — the channel that lets a scenario assert a screen is
   * *rendered* rather than merely on screen (a frozen splash screenshots
   * exactly like a working one). web: Playwright; android: the WebView's
   * DevTools socket over adb (`test/surfaces/android.ts::queryDom`).
   * Surfaces without a DOM channel leave it undefined, and the scenario
   * records a skip (`support.ts::skip`), never a silent pass.
   */
  queryDom?(peer: string, selector: string): Promise<string | null>;
  screenshot(peer: string, label: string): Promise<string | null>;
  /** Everything the peer has logged so far. */
  logs(peer: string): string;
  stop(peer: string): Promise<void>;
  teardown(): Promise<void>;
}

export interface SurfaceOptions {
  /** Harness peer ids to host (1 or 2). */
  peers: string[];
  /** Budget for `ready` and other surface-internal waits. */
  timeoutMs: number;
  /**
   * `PO_E2E_*` values the scenario needs the app to see at boot, by their
   * env-var names (`docs/device-testing.md` § "Harness seams"). Native surfaces
   * pass them into the launch environment; the web surface lowercases them into
   * `localStorage`, the only channel the browser build has.
   *
   * Per scenario, not per run: a value like a shortened pairing TTL must not
   * apply to a scenario whose flow legitimately needs the full window.
   */
  seams?: Dict;
}


