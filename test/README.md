# Tandem e2e harness

Drives the scenarios in `docs/device-testing.md` against real surfaces. Entry
point:

```bash
node test/run.ts --list                          # surfaces + scenarios + declined list
node test/run.ts --scenario smoke --surfaces web
node test/run.ts --scenario S1 --surfaces web,macos,ios-sim
node test/run.ts --scenario S1 --surfaces web,ios-device   # physical iPhone
node test/run.ts --surfaces android              # default scenario: smoke
```

Every run writes `test/runs/<ts>-<SCENARIO>-<surfaces>/` (override with
`--evidence-dir`): per-peer logs, PNGs, `result.json` (git rev, surface/peer
roster, what each surface actually ran — artifact path, device id, whether the
install was skipped — per-step outcomes, and a marker timeline that records what
each assertion actually saw — negative assertions included). Timeouts screenshot + dump the log
tail; the harness never hangs.

## Surfaces

| Surface | Driver | Peers |
|---|---|---|
| `web` | Playwright chromium, two isolated contexts | a, b |
| `macos` | built app binary + `open 'po://…'`, `screencapture` | a (default dir), b (`PO_INSTANCE=b`) |
| `ios-sim` | `xcrun simctl` (iPhone 18 Pro), `SIMCTL_CHILD_*` env | a |
| `ios-device` | `xtool` (sign+install) + `xcrun devicectl` (launch/env/screenshot), `DEVICECTL_CHILD_*` env | a |
| `android` | `adb` (Medium_Phone AVD); seams ride the launch intent's `po_e2e_*` extras | a |

Which optional methods each surface implements, and which channel carries its markers, is
in [docs/device-testing.md § Per-surface capability matrix](../docs/device-testing.md#per-surface-capability-matrix).

Builds run automatically (`dx build --platform <p>`) when the artifact under
`target/dx/po/debug/` is missing **or older than the newest source file** — a
stale bundle would otherwise keep running yesterday's code while the source
moved on, silently invalidating the run. The freshness check and the
build-then-assert-artifact-exists sequence are one shared guard,
`test/lib/build.ts::ensureBuilt`; each surface passes it only its own build step.

The iOS **device** build goes to `target/dx-e2e/` instead:
dx writes the simulator and device bundles to the same
`target/dx/po/debug/ios/` directory regardless of the triple, so a device build
would clobber the simulator one (and vice versa).

### Physical iPhone prerequisites

The pinned device is the **iPhone SE (3rd generation)**, UDID
`00008110-001A44C63E10401E` (set in `test/surfaces/ios-device.ts`). Other
phones paired to the same Mac are ignored.

```bash
xtool auth login                 # one-time Apple ID sign-in (free team works)
xcrun devicectl list devices     # the SE must not read "unavailable"
```

**USB for the install, Wi-Fi for everything else.** The app must be signed by
xtool (dx cannot: it only signs the outer `.app` and can only find an
Xcode-managed identity this repo deliberately does not use). xtool reaches
devices through usbmuxd, and on macOS 15+ the system usbmuxd has no lockdown
pair-record store — CoreDevice owns pairing — so `xtool devices --network`
cannot complete a Wi-Fi lockdown connection even when the phone advertises
`_apple-mobdev2._tcp` and devicectl holds a live tunnel. The install
therefore needs the cable.

Once the app is on the phone, the cable can come out: launch, env injection,
screenshots and log capture all go through `devicectl`, which works over
`localNetwork`. The install is skipped when the phone already holds **this
build** — the surface stamps each install into
`target/dx-e2e/installed-ios-device.json` and compares the binary hash, because
"an app is installed" was how a run silently exercised a build hours out of
date. So a Wi-Fi-only run works as long as one USB install of the current
build has happened.

If the phone is neither USB-connected nor Wi-Fi-reachable, the surface fails
with that guidance rather than hanging.

**The phone must be unlocked at launch time.** `devicectl` refuses to start an
app on a locked phone (`FBSOpenApplicationServiceErrorDomain` / `Locked`), and the
console bridge dies silently. The surface checks `devicectl device info lockState`
before launching and fails immediately with that guidance. iPhone Mirroring
cannot unlock the phone — it pauses its connection the moment the phone locks —
so unlock on the device itself (Face ID or passcode) before a run.

Note: a locked phone's `devicectl` screenshots are **black**, not an error. The
capture succeeds; the framebuffer is simply empty.

## Playwright

The harness uses the repo root's `playwright` devDependency — run `npm ci` at
the repo root, then install the browser once:

```bash
npm ci
npx playwright install chromium
```

The browser lands in the platform cache (`~/Library/Caches/ms-playwright` on
macOS, `~/.cache/ms-playwright` on Linux).

## Scenario status

`node test/run.ts --list` prints the registered set and the deliberately-not-automated
cases; what each scenario asserts is in
[docs/device-testing.md § Tandem scenario catalog](../docs/device-testing.md#tandem-scenario-catalog).

`node test/run.ts --list --names [--surfaces <list>]` prints the same set as bare names,
one per line — the form a workflow reads to build its scenario matrix. A scenario marked
`nativeOnly` in `test/scenarios/registry.ts` is omitted when the requested surfaces are
web only (`S18`: a web peer cannot observe LAN locality). Nothing else changes about
`--list`.

- Every scenario needs the `e2e` marker channel: `PO_E2E=1` in a native launch env, and
  `localStorage.po_e2e = "1"` on web (which the web surface sets itself). Without it a
  wait fails as `SEAM MISSING` instead of timing out, so a marker-less surface is
  diagnosed rather than blamed on the app.
- The anchor must be `web`: signing in and opening the share screen need the account
  password typed into a DOM, and only web has one. `macos`, `ios-sim` and `ios-device`
  join from their launch environment (`PO_E2E_PAIR_*`), so no typing channel is needed
  on the native side.
- `android` has no environment channel — `adb shell am start` cannot set env for the app it
  starts — so its seams ride the launch intent's `po_e2e_*` extras and its marker channel is
  on like the other native surfaces; it exposes `identity`, `restart`, `relaunch` and a
  CDP-backed `queryDom` DOM read (the debuggable WebView serves DevTools over adb).
- `S6` and `S9` case 3 are not automated; `--list` names the missing observable for each.
- `S18` (LAN locality) runs on the native surfaces only: it records a skip for every web
  peer, because WASM Veilid has no UDP and shares the host's network stack, so a web
  peer's locality fields say nothing about this LAN.
- **A skip is not a pass.** A step a surface cannot run is recorded, never thrown
  (`support.ts::skip`) — a native peer has no `signIn`, so `S3` legitimately skips there.
  `result.json` carries `skipped` next to `ok`, and `run.log` ends with
  `result: PASS — NOT coverage: N skipped (…), those steps asserted nothing`. A green `ok`
  with skips is a scenario that ran; it is not one that asserted anything.

### One joiner per run

`S1` — and `pickPeers`, which the other tandem scenarios share — uses **`surfaces[0]` as
the anchor and `surfaces[1]` as its single joiner**. Listing a third surface is not an
error, but that surface is only *launched*: it never pairs, and its evidence is an idle
app. A run that proves two different native joiners therefore has to happen twice:

```bash
node test/run.ts --scenario S1 --surfaces web,macos
node test/run.ts --scenario S1 --surfaces web,ios-sim      # not both in one run
```

`surfaces[2]` and beyond are ignored by the scenario, so `--surfaces web,macos,ios-sim`
passes while only proving web ↔ macOS.

**More peers than surfaces** is one scenario's layout, and it is explicit: a scenario with
`minPeers` above the listed surface count takes the extra peers on its FIRST surface
(`test/run.ts::planOwnership`) — the DOM-capable one, which can drive any number of
isolated contexts where a device cannot. `S17` is the only user (`minPeers: 3`): its first
surface hosts the anchor and the counterpart account, and the joiner is the last peer — a
third context on that surface, or the device surface when one is listed.


## Observing markers

Native logs carry ANSI colour codes, including inside the marker field
(`e2e:<esc>[3mevent<esc>[0m<esc>[2m=<esc>[0m"paired"`), so the harness strips
them before matching `event=<name>` and before parsing `key=value` fields.
A marker that looks present in the raw log but is "missing" to the harness is
almost always this.

## Secrets

Pairing links embed the PIN; `test/lib/run.ts::maskSecrets` masks
`pin=`/`password=` values, `data=` payloads, and the pairing share URL
(`https://po-ai.pages.dev/p/<payload>`) in every log/result file.

Mask the assembled line, not the bare value: the rules are prefix-anchored,
so `ctx.mask(share.pin)` matches nothing and the PIN lands in the evidence.
