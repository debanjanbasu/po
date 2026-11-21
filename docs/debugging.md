# Debugging oo — live logs, data capture, and failure triage

**Owns:** `src/components/overlays/`, `src/storage/background_errors.rs`

How to observe the app while it runs, on every platform — written for both
humans and coding agents. When a bug spans two devices (e.g. pairing), open
one capture channel **per device first**, then reproduce.

## TL;DR per platform

| Platform | Live log command |
|----------|------------------|
| macOS (dev) | `~/.cargo/bin/dx serve --platform macos --hot-patch false` — logs land in the terminal |
| macOS (installed .app) | `/Applications/oo.app/Contents/MacOS/oo` from a terminal (stdout), or `log stream --predicate 'process == "oo"' --level info` |
| iOS simulator | `xcrun simctl launch --console booted com.debanjanbasu.oo`, or `xcrun simctl spawn booted log stream --predicate 'process == "oo"'` |
| iOS device | `xcrun devicectl device process launch --console --device <UDID> <installed-app-id>` (streams stdout live; see below for the xtool app-id) |
| Android | `adb logcat -v time RustStdoutStderr:D '*:S'` (Rust stdout/stderr) or `adb logcat --pid $(adb shell pidof com.debanjanbasu.oo)` |
| Web | Browser devtools console (`dx serve --platform web`, then F12) |

Native targets initialize `tracing_subscriber::fmt` directly in `src/main.rs` at `INFO` to stdout, with `veilid_core` capped at `WARN` unless `RUST_LOG` explicitly targets it. Web initializes `tracing-wasm` at `INFO`; every event reaches the browser console (the e2e harness tails it). There is no file logger; capture stdout or the platform console.

## In-app: the Activity panel (first stop, all platforms)

The sync icon in the top-right opens the **Activity** modal:

- **Background Errors** — non-fatal failures with a `domain` tag
  (`auth.pairing`, `sync.queue`, `user.identity_watch`, `identity.watcher`,
  `user.record`, …), oldest first. This is the fastest signal on a device
  you can't attach a terminal to (e.g. an iPhone in the field).
- **Queue / Dead-letter** — pending sync operations and ops that exhausted
  retries. A stuck spinner almost always means something is visible here.
- **Background tasks** — long-running work currently in flight.

When reporting a bug, screenshot this panel on **both** devices before
restarting anything — restarts clear in-memory error history.

Entries are printed with the **whole error chain**, not just the outermost
context: `report_nonfatal` formats with `{:#}`, so an `anyhow` chain reads
`failed to open DHT record: no node answered the open` instead of just
`failed to open DHT record`. The cause after the colon is the part that
identifies the failure — read it before blaming a device or a peer.

## Android 17 local-network permission

API 37 requires runtime `ACCESS_LOCAL_NETWORK` for LAN peer traffic.
`MainActivity` requests it at launch. Denial leaves local-first startup and
Internet networking available while Android blocks LAN peer traffic. Grant it in
system settings and resume the app to restore LAN access; verify both grant and
denial flows on Android 17.

## iOS device workflow (physical iPhone)

Deploys go through xtool because dx alone can't sign for device — see the
header comment in [`scripts/ios-device-deploy.sh`](../scripts/ios-device-deploy.sh):

```bash
~/.cargo/bin/dx build --ios --target aarch64-apple-ios
./scripts/ios-device-deploy.sh
```

xtool installs the app under a namespaced id `XTL-<team>.com.debanjanbasu.oo`.
To find it and stream its console:

```bash
xcrun devicectl list devices                       # get <UDID>
xcrun devicectl device info apps --device <UDID> | grep -i oo   # installed app id
xcrun devicectl device process launch --console --device <UDID> <app-id>
```

`--console` keeps the terminal attached to the app's stdout — this is the
live iPhone log. For after-the-fact capture: `sudo log collect --device`
then open the `.logarchive` in Console.app.

**`LockdownClient.Error.muxError` from xtool** — the deploy script probes one transport at a time, preferring USB (`xtool devices --usb --no-wait`) and falling back to Wi-Fi (`--network`) only when USB is not visible. It installs with that selected mode instead of `--all`, which can fail on stale Wi-Fi usbmux entries before trying USB. If installation over the selected mode fails, the script stops rather than retrying another transport; connect/unlock the phone and rerun to select USB, or enable Finder Wi-Fi sync for cable-free installs.

**`0xe8008018` (identity no longer valid) from `devicectl install`** —
the app was signed with a revoked certificate. xtool revokes Xcode-managed
Apple Development certs when it mints its own, so don't sign iOS builds
with the keychain identity Xcode created — always deploy through the
script (xtool signs with its own identity).

## Apple scene and SDK verification

The Apple deployment minimum lives in `.cargo/config.toml` (`IPHONEOS_DEPLOYMENT_TARGET`)
and the `Info.*.plist` files; the Xcode and SDK versions come from whatever the runner
image or your local install provides.
iOS declares `UIApplicationSceneManifest`. The locked Tao version enables its
scene path only when `UIApplicationSupportsMultipleScenes` is true, and registers
its scene delegate during scene configuration. `main.rs` constructs Tao's event
loop first; `DeepLinkPlugin.prepareForLaunch` then installs targeted callbacks
before that loop invokes UIKit. Do not move bridge initialization into a mounted
component or restore process-wide Objective-C class scanning.

Swift compilation verifies API/actor compatibility, not UIKit delivery. Before
release, exercise iOS 27 devices at the minimum supported deployment target:

1. Normal launch plus terminated-app and already-running `oo://` URL opens.
2. Release-signed universal links for both pairing and contacts, including
   already-authenticated handling. Development `XTL-<team>` signing does not
   establish associated-domain delivery.
3. Background/foreground recovery, iPad scene transitions, and sharing from the
   foreground window. A background scene must not become a presentation target.
4. Camera permission grant/denial, cancellation while authorization is pending,
   and immediate reopen. AVFoundation start/stop runs on one serial queue;
   cancellation must not leave capture running.
5. Account switches and cold links with saved route/theme/nav preferences.
   Failed local hydration must remain visible without overwriting stored state.

Use a specifically identified simulator/device, not a blanket `booted` target
when other developers or captures may be using the host.

## Veilid failure signatures

Real errors from past debugging and what they actually mean:

| Error text | Root cause | Where to look |
|------------|-----------|---------------|
| `cannot decrypt value data: missing encryption key` | Record opened without its at-rest encryption key. Historically: the V1 pairing QR omitted the key embedded in `RecordKey` | `src/user/pairing_crypto.rs` (QR payload), `src/veilid/store/` |
| `Invalid argument: 'record not open'` | A write raced a close — some path closed the record while a queued op held its key | per-record locks + pins in `src/veilid/store/` |
| `Generic: value is not writable` | Record opened read-only (no writer keypair) and then written | opening call site; pins must carry the session keypair |
| `failed to re-publish identity subkey N` | Republish couldn't confirm network coverage — usually attachment still warming up or offline | network state in the Activity panel; retry after attach |
| App starts but **nothing happens**: window renders, no `event=` markers, `api_startup` never returns, no crash report, main thread idling in `-[NSApplication run]` | A **stale nightly**. Measured 2026-11-09: `1.102.0-nightly (f5cedb83a, Nov 7)` produced a macOS release binary whose `api_startup` never returned when built with `lto` + `e2e-seams`; the cell worked on `5b7b37aef` (Nov 8) with **identical source**. Debug builds and LTO-off builds worked, which is what makes it look like app code | `rustup update nightly` **first**, then `rustc -V`, then re-test. `rust-toolchain.toml` pins the rolling channel, so CI always installs the newest nightly and never sees this — only a local toolchain can be stale enough to hit it |
| Pairing device: `couldn't reach this account's sync record` | The scanning device can't fetch/decrypt the identity record — check the *sharing* device's Background Errors for the paired failure | both devices' Activity panels |

Sequence rule of thumb: Veilid owns DHT sequence numbers and conflict
detection (`set_dht_value` returns the newer value when superseded). The app
must never track its own DHT sequence state — see
[dht-architecture.md](dht-architecture.md) and [watch-system.md](watch-system.md).

## Local data: inspect or reset to a cold device

See [README.md § Data Directories](../README.md#data-directories) for the canonical per-platform `.veilid/` and browser IndexedDB storage paths.

**Cold-state reset** (reproduce "brand-new device" bugs, e.g. first pairing):

- Desktop: quit the app and remove the Veilid storage root for this identity.
- iOS device/simulator and Android: uninstall the app, then redeploy.
- Web: clear this site's browser storage.

Pairing bugs are cold-state bugs by definition — always test the scanning
side from a cold state before concluding anything.

### Web: the browser refused site storage

The web build's local store sits on two browser stores, and the *key* store is the
weaker one:

| What | Where | Why it matters |
|---|---|---|
| Device encryption key | `localStorage` — Veilid's wasm protected store picks it for a Window context | Per-origin quota of a few MB, and the store a browser refuses first |
| Local rows (`TableDB`) | IndexedDB, via `keyvaluedb-web` | Large quota; evicted with the origin, not on its own schedule |

Nothing can be persisted if that first key write is refused, so
`run_local_init` probes `localStorage` **before** Veilid builds the config that
chooses the backend (`platform::browser_storage`) — the last point at which the
reason is still readable, because Veilid's own mapping formats
`JsValue::as_string()`, which is `None` for a `DOMException`, leaving
`failed to write local storage: ` with an empty tail. The refusal is classified
permanent (`app::retry::is_permanent_local_error`), so startup hands back the
login route with the reason in the Activity panel instead of holding the splash
forever.

Symptoms, and what each one means:

- *"This browser's storage for this site is full…"* — the origin's quota is
  exhausted (`QuotaExceededError`).
- *"This browser is not giving the app access to site storage…"* — no
  `localStorage` at all: a private window, or cookies/site data blocked.
- *"This browser refused to save the app's data (browser error `SecurityError`)…"*
  — the write was blocked; the named `DOMException` is the detail the Veilid
  mapping above throws away.

Reproduce by opening the app in a private window, or by blocking cookies and site
data for the origin, then reloading. Clearing the site's storage is the reset
from the section above.

Not covered yet, and deliberately left visible rather than papered over: a browser
that permits `localStorage` but refuses IndexedDB still fails later with Veilid's
generic `failed to commit a transaction to IndexedDB`; and a device key that is
lost while its rows survive still takes Veilid's default
`wipe_on_invalid_device_encryption_key` path (wipe every table, log-only), because
the app has no typed signal for that branch — see the round's notes.

## Two-device debugging recipe (pairing, sync)

1. Start captures on both sides *before* reproducing:
   - Mac: run from terminal (`dx serve` or the raw binary).
   - iPhone: `devicectl … launch --console` (above).
2. Reproduce once. Note wall-clock time of each user action.
3. Collect: both terminal logs + both Activity → Background Errors panels.
4. Correlate by timestamp; the device that *fails* is often reacting to bad
   data published by the *other* one — read both sides before blaming one.
5. Narrow with a cold-state reset on the failing side and retest.

## Rules for agents

- **Do not add another logging crate or file logger to debug an issue.** Use the existing native `tracing_subscriber` setup in `src/main.rs` and the stdout channels above; remove temporary instrumentation before finishing.
- **Do not patch a local Veilid checkout to debug the app.** `Cargo.toml` uses the project fork `https://gitlab.com/debanjanbasu/veilid.git`, branch `main`; inspect the resolved source under `~/.cargo/git/checkouts/veilid-*/` instead.
- Surface diagnostics to users through `crate::nonfatal!` / `report_nonfatal(domain, context, err)` so they appear in the Activity panel — that is the supported, cross-platform "log sink".
For build verification commands, use [README.md § Build Verification](../README.md#build-verification).
