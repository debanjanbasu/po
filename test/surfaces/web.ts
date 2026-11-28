// oxlint-disable max-lines -- one file per surface: the harness keeps each device driver whole
// Web surface — Playwright chromium, two isolated contexts = two peers.
//
// The app is a static dx web build served from target/dx/po/release/web/public.
// WASM tracing goes to the browser console, so `Surface.logs()` is backed by
// Playwright's console events; the pairing share is read from the QR card DOM
// (web has no stdout for the event=share seam).
import { mkdirSync, writeFileSync } from 'node:fs';
import { readFile, stat } from 'node:fs/promises';
import http from 'node:http';
import { extname, join, resolve, sep } from 'node:path';
import { chromium, type Browser, type BrowserContext, type Page } from 'playwright';
import { exec } from '../lib/proc.ts';
import { ensureBuilt, E2E_SEAM_FEATURE } from '../lib/build.ts';
import { surfaceShotPath } from '../lib/run.ts';
import { readIdentityMarker, sleep, waitFor, waitForMarkerFields } from '../lib/wait.ts';
import type { Dict, DomOp, PairingFormValues, SurfaceOptions } from '../lib/types.ts';

/** The account's second factor — the PIN the harness signs up with. */
const ACCOUNT_PIN = '123456';

interface Peer {
  id: string;
  context: BrowserContext;
  page: Page;
  consoleLines: string[];
  logPath: string;
}

function webBuildDir(): string {
  return join(process.cwd(), 'target', 'dx', 'po', 'release', 'web', 'public');
}

declare global {
  interface Navigator {
    /**
     * The browser's async clipboard. `tsconfig` has no DOM lib, so Node's
     * `Navigator` type lacks the member the page-side script uses.
     */
    readonly clipboard: { readText(): Promise<string> };
  }
}

// oxlint-disable-next-line max-lines-per-function -- build/server/browser/peer setup is one ordered sequence
export async function create(
  runCtx: { dir: string; note(message: string): void; rel(path: string): string; mask(t: string): string },
  opts: SurfaceOptions,
): Promise<WebSurface> {
  const buildDir = webBuildDir();
  await ensureBuilt({
    // The HTML entry point is both the freshness artifact and the file the
    // static server requires; a manifest without it is not a usable web build.
    artifact: join(buildDir, 'index.html'),
    what: 'web build',
    note: runCtx.note.bind(runCtx),
    hint: 'dx build --platform web produced no index.html',
    build: async () => {
      const res = await exec(join(process.env.HOME ?? '', '.cargo', 'bin', 'dx'), ['build', '--release', '--features', E2E_SEAM_FEATURE, '--platform', 'web'], {
        cwd: process.cwd(),
        timeoutMs: 3_600_000,
      });
      if (res.code !== 0) throw new Error(`dx build --platform web failed:\n${res.stderr}`);
    },
  });

  // Static file server: one small http server, no dependency.
  const server = await serveStatic(buildDir);
  const browser = await chromium.launch({ headless: true });
  const peers: Peer[] = [];
  /* oxlint-disable no-await-in-loop -- one isolated browser context per peer, created in order */
  for (const id of opts.peers) {
    // isolated storage per peer
    const context = await browser.newContext();
    // The share screen's "Copy" button puts the pairing deep link on the
    // clipboard; reading it back is the only DOM-visible copy of the payload.
    await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: `http://127.0.0.1:${server.port}` });
    // The browser has no environment channel: `std::env::var` is a stub on
    // wasm, so the app reads its e2e switch from `localStorage.po_e2e`.
    // The init script runs before any page script, so the key is already set
    // when the app checks it.
    await context.addInitScript((seams: Record<string, string>) => {
      try {
        localStorage.setItem('po_e2e', '1');
        // Scenario seams arrive as `PO_E2E_*` env names (the native channel) and
        // become the lowercase `po_e2e_*` storage keys the wasm accessors read.
        for (const [key, value] of Object.entries(seams)) {
          localStorage.setItem(key.toLowerCase(), value);
        }
      } catch {
        // Storage can be unavailable at init-script time; setup() re-asserts it.
      }
    }, opts.seams ?? {});
    const page = await context.newPage();
    const consoleLines: string[] = [];
    page.on('console', (msg) => {
      consoleLines.push(`${new Date().toISOString()} ${msg.type()}: ${msg.text()}`);
    });
    page.on('pageerror', (err) => {
      consoleLines.push(`${new Date().toISOString()} pageerror: ${err.message}`);
    });
    peers.push({ id, context, page, consoleLines, logPath: join(runCtx.dir, 'web', `${id}.log`) });
  }
  /* oxlint-enable no-await-in-loop */
  return new WebSurface(server, browser, peers, opts, runCtx);
}

/** Minimal static server with SPA-ish fallback to index.html. */
// oxlint-disable-next-line max-lines-per-function -- MIME table + request handler + lifecycle kept together
async function serveStatic(root: string): Promise<{ port: number; close(): Promise<void> }> {
  const types: Dict = {
    '.html': 'text/html',
    '.js': 'text/javascript',
    '.wasm': 'application/wasm',
    '.css': 'text/css',
    '.png': 'image/png',
    '.ico': 'image/x-icon',
    '.svg': 'image/svg+xml',
    '.woff2': 'font/woff2',
    '.webp': 'image/webp',
  };
  const rootPath = resolve(root);
  // The listener must return void; the per-request work stays async inside.
  const server = http.createServer((req, res) => {
    void (async () => {
      try {
        const url = (req.url ?? '/').split('?')[0];
        let path = resolve(rootPath, `.${url}`);
        if (path !== rootPath && !path.startsWith(`${rootPath}${sep}`)) {
          res.writeHead(403, { 'content-type': 'text/plain; charset=utf-8' });
          res.end('Forbidden');
          return;
        }
        try {
          if ((await stat(path)).isDirectory()) path = join(path, 'index.html');
        } catch {
          path = join(rootPath, 'index.html');
        }
        const body = await readFile(path);
        res.writeHead(200, {
          'content-type': types[extname(path)] ?? 'application/octet-stream',
          // COOP/COEP: veilid wasm may want shared memory; keep permissive-but-safe defaults.
          'cross-origin-opener-policy': 'same-origin',
        });
        res.end(body);
      } catch (e) {
        console.error('web static server request failed:', e);
        res.writeHead(500, { 'content-type': 'text/plain; charset=utf-8' });
        res.end('Internal server error');
      }
    })();
  });
  const { promise: listening, resolve: onListening } = Promise.withResolvers<void>();
  server.listen(0, '127.0.0.1', onListening);
  await listening;
  const addr = server.address();
  const port = typeof addr === 'object' && addr ? addr.port : 0;
  return {
    port,
    close: () => {
      const { promise, resolve: onClose } = Promise.withResolvers<void>();
      server.close(() => {
        onClose();
      });
      return promise;
    },
  };
}

export class WebSurface {
  readonly name = 'web';
  readonly caps = { env: false, deepLink: false };
  readonly build: Dict = {};
  private server: { port: number; close(): Promise<void> };
  private browser: Browser;
  private peers: Peer[];
  private opts: SurfaceOptions;
  private runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string };

  constructor(
    server: { port: number; close(): Promise<void> },
    browser: Browser,
    peers: Peer[],
    opts: SurfaceOptions,
    runCtx: { dir: string; note(m: string): void; rel(p: string): string; mask(t: string): string },
  ) {
    this.server = server;
    this.browser = browser;
    this.peers = peers;
    this.opts = opts;
    this.runCtx = runCtx;
  }

  get peerIds(): string[] {
    return this.peers.map((p) => p.id);
  }

  private peer(id: string): Peer {
    const peer = this.peers.find((p) => p.id === id);
    if (!peer) throw new Error(`web surface has no peer ${id}`);
    return peer;
  }

  private url(): string {
    return `http://127.0.0.1:${this.server.port}/`;
  }

  async setup(): Promise<void> {
    // Warm each page once; console capture starts before navigation.
    await Promise.all(
      this.peers.map(async (p) => {
        await p.page.goto(this.url(), { waitUntil: 'domcontentloaded' });
      }),
    );
    // The init script usually set the key before the first script ran, but a
    // browser may refuse storage at that point (opaque origin, blocked
    // storage). Re-assert it after navigation and reload when it was missing,
    // so the app's marker channel is on for the whole run.
    await Promise.all(
      this.peers.map(async (p) => {
        const key = await p.page.evaluate(() => {
          try {
            return localStorage.getItem('po_e2e');
          } catch {
            return null;
          }
        });
        if (key === '1') return;
        this.runCtx.note(`${p.id}: localStorage.po_e2e missing — re-setting and reloading`);
        await p.page.evaluate(() => {
          try {
            localStorage.setItem('po_e2e', '1');
          } catch {
            // reported by the assertion below
          }
        });
        const now = await p.page.evaluate(() => {
          try {
            return localStorage.getItem('po_e2e');
          } catch {
            return null;
          }
        });
        if (now !== '1') {
          throw new Error(`${p.id}: cannot set localStorage.po_e2e — the app will emit no event= markers`);
        }
        await p.page.reload({ waitUntil: 'domcontentloaded' });
      }),
    );
  }

  async launch(_peer: string, _env: Dict): Promise<void> {
    // env seams do not exist in the browser; nothing to inject.
  }

  async ready(peer: string): Promise<string> {
    const p = this.peer(peer);
    await waitFor(
      async () => ((await p.page.locator('#login-username-input').count()) > 0 ? 'login' : null),
      { label: `${peer} login screen`, timeoutMs: this.opts.timeoutMs },
    );
    return 'login screen rendered (#login-username-input present)';
  }

  async openUrl(peer: string, url: string): Promise<void> {
    // A browser cannot handle a custom scheme: `page.goto('po://…')` fails
    // with ERR_UNKNOWN_URL_SCHEME and there is no protocol handler to
    // register. A CONTACT link has a real substitute — the app's own paste
    // path parses the same link — so deliver it there.
    if (url.startsWith('po://chat')) {
      await this.addContact(peer, url, this.opts.timeoutMs);
      return;
    }
    if (url.startsWith('po://')) {
      // Anything else has no web channel at all: the app reads launch URLs
      // only on iOS/macOS/Android (`platform::deep_link::pending_url()` is
      // empty elsewhere), and the contact dialog only parses contact links.
      // Saying so beats a 120 s timeout inside an unrelated DOM wait.
      throw new Error(
        `web cannot deliver ${url.split('?')[0]} — the app has no launch-URL source in a browser`,
      );
    }
    await this.peer(peer).page.goto(url, { waitUntil: 'domcontentloaded' });
  }

  /**
   * Run a raw DOM flow. The generic driver: a scenario for a screen with no
   * purpose-built method (TOTP, Security, Snapshots, Account) does not have
   * to add one to this file, which would make every new scenario edit the
   * same shared surface.
   */
  async dom(peer: string, ops: DomOp[]): Promise<Record<string, string | number>> {
    const page = this.peer(peer).page;
    const out: Record<string, string | number> = {};
    /* oxlint-disable no-await-in-loop -- DOM ops run in the order the scenario wrote them */
    for (const op of ops) {
      if ('click' in op) {
        await page.locator(op.click).first().click();
      } else if ('fill' in op) {
        await page.locator(op.fill[0]).first().fill(op.fill[1]);
      } else if ('waitFor' in op) {
        await page.locator(op.waitFor[0]).first().waitFor({ state: 'visible', timeout: op.waitFor[1] });
      } else if ('readText' in op) {
        out[op.readText[0]] = (await page.locator(op.readText[1]).first().innerText()).trim();
      } else if ('readCount' in op) {
        out[op.readCount[0]] = await page.locator(op.readCount[1]).count();
      } else if ('reload' in op) {
        // The WASM app reboots; wait for the document rather than a selector,
        // so the caller decides what it expects to find afterwards.
        await page.reload({ waitUntil: 'domcontentloaded' });
        // A settling beat, deliberately not a condition: the ops that follow a
        // reload assert the *absence* of screens (S13 reads #po-main-dock and
        // #pin-gate-input expecting 0), so any "a screen is up" probe would
        // return on a transient boot render and turn the assertion into a race.
        await page.waitForTimeout(2_000);
      } else if ('download' in op) {
        const [saved] = await Promise.all([
          page.waitForEvent('download'),
          page.locator(op.download[1]).first().click(),
        ]);
        const dir = join(this.runCtx.dir, 'downloads');
        mkdirSync(dir, { recursive: true });
        const target = join(dir, saved.suggestedFilename());
        await saved.saveAs(target);
        out[op.download[0]] = target;
      } else if ('setFiles' in op) {
        await page.locator(op.setFiles[0]).first().setInputFiles(op.setFiles[1]);
      }
    }
    /* oxlint-enable no-await-in-loop */
    return out;
  }

  /**
   * The rendered DOM, for scenarios that assert a screen is present: what
   * `selector` matched, or null. The android surface answers the same shape
   * over CDP (`test/surfaces/android.ts::queryDom`), so one scenario asserts
   * the same first screen on both.
   */
  async queryDom(peer: string, selector: string): Promise<string | null> {
    const page = this.peer(peer).page;
    const locator = page.locator(selector).first();
    if ((await locator.count()) === 0) return null;
    const [id, label, text] = await Promise.all([
      locator.getAttribute('id'),
      locator.getAttribute('aria-label'),
      locator.textContent(),
    ]);
    const parts = [
      id === null ? null : `#${id}`,
      label === null ? null : `[aria-label="${label}"]`,
      text === null || text.trim() === '' ? null : JSON.stringify(text.trim().slice(0, 80)),
    ].filter((part) => part !== null);
    if (parts.length > 0) return parts.join(' ');
    // No readable attributes: the tag name still says *what* matched. The
    // tsconfig has no DOM lib (Node types stop short of `document`), so take
    // an ElementHandle — its `nodeName` surfaces through the protocol as a
    // plain string — and never touch the page's globals from the host.
    const handle = await locator.elementHandle();
    if (handle === null) return null;
    try {
      const raw: unknown = await handle.jsonValue();
      if (typeof raw !== 'string' || raw === '') return null;
      return `<${raw.toLowerCase()}>`;
    } finally {
      await handle.dispose().catch(() => {});
    }
  }

  async fillPairingForm(peer: string, values: PairingFormValues): Promise<void> {
    const page = this.peer(peer).page;
    if (values.payload !== undefined && values.payload !== '') {
      await page.locator('#pairing-payload-input').fill(values.payload);
    }
    if (values.pin !== undefined && values.pin !== '') {
      await page.getByLabel('Pairing PIN').fill(values.pin);
    }
    if (values.password !== undefined && values.password !== '') {
      await page.getByLabel('Account password').fill(values.password);
    }
  }

  /**
   * Admission ends at the PIN gate (second factor) — it can appear right
   * after onboarding or on the next route change. Answer it wherever it shows
   * up, then wait for the app dock.
   */
  private async ensureAdmitted(page: Page, timeoutMs: number): Promise<void> {
    await waitFor(async () => {
      if ((await page.locator('#pin-gate-input').count()) > 0) return 'pin';
      if ((await page.locator('#po-main-dock').count()) > 0) return 'app';
      return null;
    }, { label: 'PIN gate or app dock', timeoutMs });
    if ((await page.locator('#pin-gate-input').count()) > 0) {
      await page.locator('#pin-gate-input').fill('123456');
    }
    await waitFor(async () => ((await page.locator('#po-main-dock').count()) ? 'app' : null), {
      label: 'main app dock',
      timeoutMs,
    });
  }

  /** Anchor flow: sign up + navigate to the share screen + enter password. */
  async signIn(peer: string, username: string, password: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    await page.locator('#login-username-input').fill(username);
    await page.locator('#login-password-input').fill(password);
    // Reactive auto-login finds no local user → "No local user found." then
    // the create-account button appears.
    await page.locator('#create-account-button').click();
    // Signed-in app: the Chat page (h1 "Chat") — but the onboarding wizard
    // gates first (PostRegister overlay: terms, DOB, PIN).
    await waitFor(async () => ((await page.locator('#onboarding-terms-checkbox').count()) ? 'ob' : null), {
      label: 'onboarding wizard',
      timeoutMs,
    });
    await page.locator('#onboarding-terms-checkbox').check();
    await page.locator('#onboarding-dob-input').fill('1990-01-01');
    await page.locator('#onboarding-dob-confirm-checkbox').check();
    await page.locator('#onboarding-pin-input').fill('123456');
    await page.locator('#onboarding-complete-button').click();
    await this.ensureAdmitted(page, timeoutMs);
  }

  /** Anchor: open Account → Pair New Device, enter the account password. */
  async openShareScreen(peer: string, password: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    // Navigate in-app (the dock's Account link → "Pair New Device"): a full
    // page reload reboots the WASM app and drops the admitted session, which
    // comes back as a PIN gate instead of the share screen.
    await page.locator('#po-dock-Account').first().click();
    await waitFor(async () => ((await page.getByText('Pair New Device').count()) ? 'account' : null), {
      label: 'account page',
      timeoutMs,
    });
    await page.getByText('Pair New Device').click();
    await waitFor(async () => ((await page.getByPlaceholder('Account password').count()) > 0) ? 'form' : null, {
      label: 'share form',
      timeoutMs,
    });
    await page.getByPlaceholder('Account password').fill(password);
    // The QR needs a published identity; until the background CreateIdentity
    // op lands, the screen says so and offers a retry. The probe does the
    // retrying: a 5 s cadence, because each attempt re-runs that whole op.
    await waitFor(
      async () => {
        if ((await page.getByText('Pairing QR').count()) > 0) return 'qr' as const;
        const retry = page.getByRole('button', { name: 'Retry' });
        if ((await retry.count()) > 0) {
          await retry.first().click().catch(() => {});
        } else {
          await page.getByPlaceholder('Account password').fill(password);
        }
        return null;
      },
      { label: `${peer} pairing QR card (CreateIdentity)`, timeoutMs, intervalMs: 5_000 },
    );
  }

  /** Read the share screen's PIN + deep link (PIN from the DOM, link from the clipboard). */
  async readShare(peer: string, timeoutMs: number): Promise<{ pin: string; deepLink: string }> {
    const page = this.peer(peer).page;
    // PIN badge text: "PIN: <12 digits>" inside the share card details.
    const pinText = await waitFor(async () => {
      const el = page.getByText(/^PIN: \d{12}$/u).first();
      return (await el.count()) ? el.textContent() : null;
    }, { label: 'PIN badge', timeoutMs });
    const pin = (pinText ?? '').replace(/^PIN:\s*/u, '');
    // The deep link is the card's copy payload; the "Copy" button is the only
    // DOM control that exposes it (the QR image itself is opaque).
    await page.getByRole('button', { name: 'Copy identity to clipboard' }).click();
    const deepLink = await waitFor(async () => {
      const text = await page.evaluate(() => navigator.clipboard.readText());
      return text.includes('data=') ? text : null;
    }, { label: 'pairing deep link on the clipboard', timeoutMs });
    return { pin, deepLink };
  }

  /** Joiner: navigate to /pair (signed-out) and fill payload+pin+password. */
  async openPair(
    peer: string,
    payloadUrl: string,
    pin: string,
    password: string,
    timeoutMs: number,
  ): Promise<void> {
    const page = this.peer(peer).page;
    await page.goto(new URL('/pair', this.url()).toString(), { waitUntil: 'domcontentloaded' });
    await waitFor(async () => ((await page.locator('#pairing-payload-input').count()) ? 'form' : null), {
      label: `${peer} pairing form`,
      timeoutMs,
    });
    // The payload input normalizes on paste of the full deep link; fill it
    // directly and let the app's own validation run.
    await page.locator('#pairing-payload-input').fill(payloadUrl);
    await this.fillPairingForm(peer, { pin, password });
  }

  /** Joiner: wait for the pairing sync status banner to reach Complete. */
  async waitComplete(peer: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    const deadline = Date.now() + timeoutMs;
    let answeredPinGate = false;
    let retries = 0;
    /* oxlint-disable no-await-in-loop -- UI poll: each read must observe the previous tick */
    while (Date.now() < deadline) {
      if ((await page.getByText('Device sync complete').count()) > 0) return;
      // A fresh device can reach the anchor's identity record before the
      // anchor's copy is replicated to the network; the app surfaces that as
      // a pairing error with a Retry button. Ride it like a user would — and
      // pace the retries at 5 s, because each one re-runs the whole pairing
      // fetch against the DHT (a tight loop would spend the budget on retries).
      const alert = page.getByRole('alert');
      if ((await alert.count()) > 0) {
        const retry = page.getByRole('button', { name: 'Retry' });
        if ((await retry.count()) > 0 && retries < 6) {
          retries += 1;
          await retry.first().click().catch(() => {});
          await sleep(5_000);
          continue;
        }
      }
      // A paired device still runs the account's second factor locally. The
      // gate appears only after the pairing fetch lands, so wait for it and
      // answer it exactly once (re-filling mid-verification cancels it).
      const pinGate = page.locator('#pin-gate-input');
      if (!answeredPinGate && (await pinGate.count()) > 0) {
        await pinGate.fill('123456');
        answeredPinGate = true;
      }
      const incorrect = await page.getByText('Incorrect PIN').count();
      if (incorrect > 0) {
        throw new Error(`${peer} rejected the onboarding PIN at the second-factor gate`);
      }
      // 1 s poll cadence: a state machine (retry above, PIN gate here) rather
      // than one probe, so it keeps its own loop instead of `waitFor`.
      await sleep(1_000);
    }
    /* oxlint-enable no-await-in-loop */
    const body = (await page.locator('body').innerText()).replace(/\s+/gu, ' ').slice(0, 300);
    throw new Error(`${peer} did not reach "Device sync complete" before the timeout; screen: ${body}`);
  }

  logs(peer: string): string {
    return this.peer(peer).consoleLines.join('\n');
  }

  // ── Optional capabilities ────────────────────────────────────────────

  /** Navigate the app in-place (no reload) to the Chat list. */
  private async gotoChat(page: Page, timeoutMs: number): Promise<void> {
    // The dock's Chat entry is the stable route in; a full reload would drop
    // the admitted session into the PIN gate.
    const dockLink = page.locator('#po-dock-Chat').first();
    if ((await dockLink.count()) > 0) {
      await dockLink.click();
    } else {
      await page.goto(new URL('/chat', this.url()).toString(), { waitUntil: 'domcontentloaded' });
    }
    await waitFor(async () => ((await page.getByRole('button', { name: 'Add Contact' }).count()) > 0 ? 'chat' : null), {
      label: 'chat list',
      timeoutMs,
    });
  }

  /**
   * The peer's own identity, from the app's `event=identity` marker.
   *
   * Marker-first on purpose. The marker is emitted when the app renders this
   * device's OWN contact QR, and that is the add-contact dialog — not the
   * Account page, whose header carries no DHT key at all. So the dialog is
   * what gets opened, and the marker is the only source read.
   */
  async identity(peer: string): Promise<{ username: string; dhtKey: string }> {
    const page = this.peer(peer).page;
    await this.gotoChat(page, this.opts.timeoutMs);
    const addButton = page.locator('#chat-add-contact-button');
    await waitFor(async () => ((await addButton.count()) > 0 ? 'button' : null), {
      label: 'Add Contact button',
      timeoutMs: this.opts.timeoutMs,
    });
    await addButton.first().click();

    const { username, dhtKey } = await readIdentityMarker(
      this.name,
      peer,
      () => this.logs(peer),
      this.opts.timeoutMs,
      true,
    );

    // Close the dialog so later steps can reach the chat list again.
    await page.keyboard.press('Escape').catch(() => {});
    // Not swallowed: a dialog that will not close, or a chat list that will not
    // load, makes the next read fail far from the cause. Let the labelled
    // waitFor name it.
    await waitFor(async () => ((await page.locator('dialog[open]').count()) === 0 ? 'closed' : null), {
      label: 'add-contact dialog to close',
      timeoutMs: 10_000,
    });
    await this.gotoChat(page, this.opts.timeoutMs);
    return { username, dhtKey };
  }

  /**
   * Deliver a contact link/payload through the app's own paste path: the
   * Add Contact dialog's "Contact details" input parses `po://chat?data=…`,
   * the https share URL and raw `username|dht_key` alike.
   */
  async addContact(peer: string, link: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    await this.gotoChat(page, timeoutMs);
    const addButton = page.getByRole('button', { name: 'Add Contact' });
    await waitFor(async () => ((await addButton.count()) > 0 ? 'button' : null), {
      label: 'Add Contact button',
      timeoutMs,
    });
    await addButton.first().click();
    const input = page.getByLabel('Contact details');
    await waitFor(async () => ((await input.count()) > 0 ? 'input' : null), {
      label: 'add-contact input',
      timeoutMs,
    });
    // This peer needs its OWN identity first: signup only enqueues
    // `CreateIdentity`, and the app refuses the paste ("Complete onboarding
    // first — no DHT identity yet") until that background op lands a dht_key.
    // The open dialog is the observable — its QR resource re-runs when the
    // identity appears and emits `event=identity` — so wait for the marker
    // instead of guessing a delay.
    await waitForMarkerFields(() => this.logs(peer), 'identity', timeoutMs, true);
    await input.fill(link);
    // The app auto-submits a parseable payload; the dialog closes when the
    // contact was stored. A refusal leaves it open and reports itself in the
    // app's log — see the WARN from `storage/background_errors` — so a timeout
    // here means the paste was rejected, not that the click was missed.
    await waitFor(async () => ((await page.locator('dialog[open]').count()) === 0 ? 'closed' : null), {
      label: 'add-contact dialog to close',
      timeoutMs,
    });
  }

  /**
   * Accept the pending conversation invite. An invite from an unknown sender
   * is parked by the app until the user accepts it, so the invite markers
   * (`invite_received`, `conversation_active`) only land after this.
   */
  async acceptInvite(peer: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    await this.gotoChat(page, timeoutMs);
    const accept = page.getByRole('button', { name: 'Accept', exact: true });
    await waitFor(async () => ((await accept.count()) > 0 ? 'invite' : null), {
      label: 'pending invite accept button',
      timeoutMs,
    });
    await accept.first().click();
    // The invite row disappears once the conversation was accepted.
    await waitFor(async () => ((await accept.count()) === 0 ? 'accepted' : null), {
      label: 'pending invite to clear',
      timeoutMs,
    });
  }

  /** Open an existing conversation by the contact title shown in the list. */
  async openConversation(peer: string, title: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    await this.gotoChat(page, timeoutMs);
    const wanted = title.trim().replace(/^@/u, '').toLowerCase();
    const row = page.locator('a[href^="/chat/"]').filter({ hasText: wanted }).first();
    await waitFor(async () => ((await row.count()) > 0 ? 'row' : null), {
      label: `conversation "${title}"`,
      timeoutMs,
    });
    await row.click();
    await waitFor(async () => ((await page.locator('#chat-messages').count()) > 0 ? 'conversation' : null), {
      label: 'conversation view',
      timeoutMs,
    });
  }

  /** Type into the open conversation's composer and activate send. */
  async sendMessage(peer: string, text: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    const composer = page.locator('textarea[aria-label="Message"]');
    await waitFor(async () => ((await composer.count()) > 0 ? 'composer' : null), {
      label: 'message composer',
      timeoutMs,
    });
    await composer.fill(text);
    const send = page.getByRole('button', { name: 'Send message' });
    await waitFor(async () => ((await send.count()) > 0 ? 'send' : null), {
      label: 'send button',
      timeoutMs,
    });
    await send.first().click();
  }

  /** The rendered message bubbles of the open conversation, oldest first. */
  async readMessages(peer: string): Promise<string[]> {
    const page = this.peer(peer).page;
    const bubbles = page.locator('#chat-messages .chat-bubble');
    const count = await bubbles.count();
    const texts: string[] = [];
    /* oxlint-disable no-await-in-loop -- read the bubbles in DOM order, one at a time */
    for (let i = 0; i < count; i += 1) {
      texts.push((await bubbles.nth(i).innerText()).trim());
    }
    /* oxlint-enable no-await-in-loop */
    // The message column is `flex-col-reverse`: newest first in the DOM.
    return texts.reverse();
  }

  /** Set the Account → Profile DOB field, tick the confirmation, save. */
  async editProfile(peer: string, dob: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    await this.gotoProfile(page, timeoutMs);
    await this.fillDob(page, dob, timeoutMs);
    const confirm = page.locator('input[type="checkbox"]').first();
    await waitFor(async () => ((await confirm.count()) > 0 ? 'confirm' : null), {
      label: 'DOB confirmation checkbox',
      timeoutMs,
    });
    await confirm.check();
    // The app navigates back to the Account page once the save lands.
    await waitFor(async () => ((await page.locator('input[type="date"]').count()) === 0 ? 'saved' : null), {
      label: 'DOB save to complete',
      timeoutMs,
    });
  }

  /**
   * Set the DOB field and leave it dirty — no confirmation, no save. The app
   * only raises its DOB-conflict row while the local field is unsaved.
   */
  async stageProfileEdit(peer: string, dob: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    await this.gotoProfile(page, timeoutMs);
    await this.fillDob(page, dob, timeoutMs);
  }

  /** Fill the profile DOB date input (`YYYY-MM-DD`). */
  private async fillDob(page: Page, dob: string, timeoutMs: number): Promise<void> {
    const date = page.locator('input[type="date"]').first();
    await waitFor(async () => ((await date.count()) > 0 ? 'date' : null), {
      label: 'DOB input',
      timeoutMs,
    });
    await date.fill(dob);
  }

  /** The profile's DOB-conflict row text, or null when it is not shown. */
  async readProfileConflict(peer: string): Promise<string | null> {
    const page = this.peer(peer).page;
    const conflict = page.locator('p.text-warning').filter({ hasText: 'date of birth changed' }).first();
    if ((await conflict.count()) === 0) return null;
    return (await conflict.innerText()).trim();
  }

  /** Navigate in-app to Account → Profile (no reload: the session survives). */
  private async gotoProfile(page: Page, timeoutMs: number): Promise<void> {
    const dockAccount = page.locator('#po-dock-Account').first();
    if ((await dockAccount.count()) > 0) {
      await dockAccount.click();
    } else {
      await page.goto(new URL('/account', this.url()).toString(), { waitUntil: 'domcontentloaded' });
    }
    const profileLink = page.locator('#settings-link-profile').first();
    await waitFor(async () => ((await profileLink.count()) > 0 ? 'account' : null), {
      label: 'account page',
      timeoutMs,
    });
    await profileLink.click();
    await waitFor(async () => ((await page.locator('input[type="date"]').count()) > 0 ? 'profile' : null), {
      label: 'profile page',
      timeoutMs,
    });
  }

  /**
   * Navigate to the Pairing route without generating a share. A signed-in
   * device renders the share side there, so this is the web-observable
   * equivalent of "a pairing link opened on an already-signed-in device".
   *
   * The share side is password-gated: it asks for the account password before
   * it renders the QR. The gate has to be unlocked here, otherwise the route
   * never reaches the share card and the caller times out.
   */
  async openPairing(peer: string, password: string, timeoutMs: number): Promise<void> {
    const page = this.peer(peer).page;
    const accountLink = page.locator('#po-dock-Account').first();
    if ((await accountLink.count()) > 0) {
      await accountLink.click();
      const pairLink = page.locator('#settings-link-pairing').first();
      await waitFor(async () => ((await pairLink.count()) > 0 ? 'link' : null), {
        label: 'account page',
        timeoutMs,
      });
      await pairLink.click();
    } else {
      await page.goto(new URL('/pair', this.url()).toString(), { waitUntil: 'domcontentloaded' });
    }
    await waitFor(async () => {
      if ((await page.getByText('Pairing QR').count()) > 0) return 'share';
      if ((await page.getByLabel('Pairing PIN').count()) > 0) return 'form';
      // The share side is password-gated; answer the gate and keep waiting.
      const gate = page.getByPlaceholder('Account password');
      if ((await gate.count()) > 0) {
        await gate.fill(password).catch(() => {});
        const submit = page.getByRole('button', { name: 'OK' });
        if ((await submit.count()) > 0) await submit.first().click().catch(() => {});
      }
      return null;
    }, { label: 'pairing route', timeoutMs });
  }

  /** The peer's visible page text, whitespace-collapsed. */
  async readText(peer: string): Promise<string> {
    const page = this.peer(peer).page;
    return (await page.locator('body').innerText()).replace(/\s+/gu, ' ').trim();
  }

  /**
   * Reload the peer and get it back into the app. The browser has no env
   * channel, so `env` is ignored; the app's second factor is answered with
   * the account PIN the harness signs up with.
   */
  async restart(peer: string, _env: Dict): Promise<void> {
    const page = this.peer(peer).page;
    await page.reload({ waitUntil: 'domcontentloaded' });
    const pinGate = page.locator('#pin-gate-input');
    await waitFor(async () => {
      if ((await pinGate.count()) > 0) return 'pin';
      if ((await page.locator('#po-main-dock').count()) > 0) return 'app';
      return null;
    }, { label: 'PIN gate or app dock after reload', timeoutMs: this.opts.timeoutMs });
    if ((await pinGate.count()) > 0) {
      // The gate auto-submits once the field is valid.
      await pinGate.fill(ACCOUNT_PIN);
      await waitFor(async () => ((await page.locator('#po-main-dock').count()) ? 'app' : null), {
        label: 'app shell after the PIN gate',
        timeoutMs: this.opts.timeoutMs,
      });
    }
  }

  async screenshot(peer: string, label: string): Promise<string | null> {
    const p = this.peer(peer);
    const path = surfaceShotPath(this.runCtx.dir, 'web', peer, label);
    await p.page.screenshot({ path, fullPage: true });
    return path;
  }

  async stop(_peer: string): Promise<void> {
    // contexts closed in teardown
  }

  async teardown(): Promise<void> {
    /* oxlint-disable no-await-in-loop -- peers are flushed and closed one at a time, in order */
    for (const p of this.peers) {
      mkdirSync(join(this.runCtx.dir, 'web'), { recursive: true });
      writeFileSync(p.logPath, this.runCtx.mask(p.consoleLines.join('\n')));
      await p.context.close().catch(() => {});
    }
    /* oxlint-enable no-await-in-loop */
    await this.browser.close().catch(() => {});
    await this.server.close().catch(() => {});
  }
}
