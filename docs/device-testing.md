# Device testing strategy — tandem e2e (pairing, sync, messaging)

**Owns:** `scripts/dx-setup.ts` — the host toolchain (JDK / Android SDK+NDK / `xtool`)
on macOS, Linux, and Windows; its build-side contract is
[README.md § Build Verification](../README.md#build-verification). The tandem harness lives
in the root `test/` folder (see [Phases](#phases) and
[test/README.md](../test/README.md) for the per-surface contract); the app-side seams it
relies on are `src/config.rs::e2e` and its call sites.
Read alongside [docs/debugging.md](debugging.md) (per-platform logs, cold-state reset,
two-device recipe) — this doc only adds the automation layer on top of it.

## Purpose

Prove, on real hardware, the flows that are *only* real on hardware:

* a joining device can pair into an existing account and finish hydration;
* two attached devices converge — conversation bootstrap, message send/receive,
  CRDT merges, dictionary sync, queue/DLQ behaviour;
* platform gates (local-network permission, biometrics, deep links,
  background/foreground) do not wedge that convergence.

Non-goals: replacing the build verification in [README.md § Build Verification](../README.md#build-verification),
adding a test suite to the repo (the [verification policy](../AGENTS.md) forbids that
pre-release), or hardware in CI — no hosted runner has the devices. `web` is the
exception and runs in CI: it needs no hardware, so `e2e.yml` puts the whole scenario
catalog through it while the `apple` and `android` jobs cover only the joiner path
(`S1` / `smoke`).

Unit-level logic is cheap to reason about; **tandem behaviour is the risk**, so the
harness exists to make two-plus devices observable and injectable, not to
re-implement UI clicking.

## Tool reality check (verified 2026-10-30)

| Target | Usable driver | Notes |
|---|---|---|
| iPhone (physical) | `xtool` (sign+install, **USB only**) + `xcrun devicectl` (launch/env/screenshot, USB or Wi-Fi) | Maestro explicitly does **not** support physical iOS devices (a third-party `maestro-runner` exists to bridge that; not adopted). Appium would need WebDriverAgent built and signed with an Xcode-managed Apple Development cert — the exact thing [`scripts/ios-device-deploy.sh`](../scripts/ios-device-deploy.sh) makes xtool revoke. Signing collision, avoid. |
| iPhone (simulator) | `xcrun simctl` | Same channels as `devicectl`; no signing, deterministic reset. The default iOS surface. |
| macOS desktop | `osascript` (Apple Event URL delivery), terminal stdout, `log stream` | Dioxus has no UI-test harness; Appium Mac2 driver exists (v4.3.x, maintained) but buys nothing over deep-link injection + log assertions. `open 'po://…'` is **not** a reliable channel here — the simulator build claims the same bundle id (see the LaunchServices note below). |
| Android (physical) | `adb` (input/screencap/logcat) and optionally Maestro (check its current CLI for Android support) | Most automatable target: real synthetic taps and text. `adb` comes with the Android SDK in the prerequisites table. |
| Web (Chromium) | Playwright | UI-flow logic only — see [web tandem caveat](#device-pairs-and-the-inner-loop). |

`devicectl` (verified locally) covers the whole iPhone channel:
`device install app`, `device info apps --bundle-id`, `device process launch`
(with `--payload-url`, `-e/--environment-variables`, `--console`,
`--terminate-existing`), `device capture screenshot|screen-record`,
`device pasteboard copy|paste`, `device settings biometrics`, `device info lockState`,
`device info mountpoint`. Everything except synthetic touch/keyboard.

Missing from `devicectl`: **typing**. That single gap drives the phase plan below —
either a human/agent types the two pairing fields, or the app accepts them through a
harness seam (below).

App-side surfaces the harness already has for free:

* URL schemes `po` on iOS/macOS (both `Info.plist`s) and Android's deep-link bridge;
* pairing intake accepts `po://pair?data=<payload>`, the universal link, and raw payloads
  (`src/platform/share_urls.rs`);
* contact intake accepts `po://chat?data=<base64url(username|dht_key)>`;
* send intake accepts `po://send?to=<username>&text=<message>` — the send channel for a
  surface with no typing channel. Refused unless the app's `e2e` seam is on
  (`config::e2e::enabled()`), so it exists only in debug/`e2e-seams` builds; the app's own
  send path runs it, and `deep_link_send` reports the outcome;
* native `tracing_subscriber` INFO → stdout on all three platforms, per-platform log
  commands already documented in [docs/debugging.md](debugging.md#tldr-per-platform).

## Prerequisites

Host tools, with the state found on this Mac (2026-10-30):

| Tool | Install | Needed for | State here |
|---|---|---|---|
| Xcode | App Store | `devicectl` | present |
| `xtool` | `node scripts/dx-setup.ts` + `xtool auth login` | signing/installing the iOS device build (`dx` cannot; see the script header) | installed; login pending |
| JDK | `node scripts/dx-setup.ts` | Gradle/AGP for any Android build | present, within the range Gradle accepts |
| Android SDK | `node scripts/dx-setup.ts` | `adb`, platform, NDK | complete — see below |
| Playwright | `npm ci` then `npx playwright install chromium` | the web surface — the anchor of every tandem run | installed |

Keep Playwright's package and its browser revision in step. A version bump in
`package.json` does not fetch the matching browser, and the next run then dies at
`browserType.launch` with *"Executable doesn't exist at
`…/chromium_headless_shell-<rev>`"* — the harness launches `headless: true`, which uses
the headless shell rather than the full Chromium (`npx playwright install chromium`
installs both; `npx playwright install --dry-run chromium` lists them).

Android/JDK setup is one idempotent step, `eval "$(node scripts/dx-setup.ts)"`
(canonical doc: [README.md § Build Verification](../README.md#build-verification)).
Verified state after it: a platform, build-tools, and NDK matching `[android]` in
`Dioxus.toml`, with the API-matched `aarch64-linux-android<API>-clang` wrapper `dx`
requires, and `adb` on `PATH`. `dx` has no default SDK path, so *"Android not installed properly"*
means those exports are missing from the shell. `dx` drives install/launch/logcat
itself — `dx serve|run --platform android` is the supported deploy path. Manual
fallback if a script needs the artifact path:
`target/dx/po/debug/android/app/build/outputs/apk/debug/app-debug.apk` (locked `dx`
revision `4b63317`).

Deliberately **not** adopted: Maestro (no physical iOS support, no macOS driver),
Appium + WebDriverAgent (signing collision, see [Risks](#risks-and-gotchas)), an
XCUITest project (starts a second build system for taps the URL/env channel already
avoids), and a simulator/emulator tier for now
([why](#simulators-and-emulators-in-scope-hardware-is-the-release-gate)).

Device-side, once per device:

* **iPhone** — the SE (3rd generation), UDID `00008110-001A44C63E10401E`. Developer Mode on
  (Settings → Privacy & Security → Developer Mode); trust the xtool developer certificate
  (Settings → General → VPN & Device Management); phone unlocked for install/launch
  (`devicectl` reports a locked phone as `unavailable`, and
  `xcrun devicectl device info lockState --device <UDID>` is the cheap pre-flight); grant
  the Local Network prompt when it appears. The **install** needs the USB cable; the run
  itself works over Wi-Fi — see
  [iPhone: USB for the install, Wi-Fi for the run](#iphone-usb-for-the-install-wi-fi-for-the-run).
* **Android** — USB debugging on and the host key authorized (`adb devices` shows
  `device`, not `unauthorized`); grant `ACCESS_LOCAL_NETWORK` at the prompt. The device
  needs **Google Play services**, not the Play Store app: the QR scanner delegates to
  GMS (`play-services-code-scanner` → ML Kit `GmsBarcodeScanning`, whose implementation
  "resides entirely within Google Play services"), and because the app does not request
  install-time module download, GMS fetches the scanner module on first use — so a
  GMS-free device (AOSP, de-Googled ROM) can never scan, and the first scan needs
  network. The emulator images used by the harness and by `e2e.yml`
  (`google_apis*`) include GMS and omit the Store app, which nothing in the repo uses —
  there is no push-notification code and no updater.
* **Both** — same LAN, no VPN, and the pairing PIN/password for the test account to hand.

Sanity check before a run:

```bash
node scripts/dx-setup.ts --self-check          # host toolchain maps, no I/O
xcrun devicectl list devices                       # iPhone must read available
xcrun devicectl device info lockState --device <UDID>
zsh -lic 'java -version'                           # must be a JDK Gradle accepts
adb devices                                        # after platform-tools is on PATH
```

## The three channels

**1. Inject — launch URLs, args, env.** No UI driver required:

```bash
# iPhone: installed dev builds are namespaced XTL-<team>.com.debanjanbasu.po
xcrun devicectl list devices
xcrun devicectl device info apps --device <UDID> --search po
xcrun devicectl device process launch --console --terminate-existing \
  --device <UDID> --payload-url 'po://pair?data=<payload>' \
  -e '{"RUST_LOG":"info,veilid_core=warn"}' <installed-app-id>
# caller-env prefix variant, when the JSON form is awkward:
DEVICECTL_CHILD_RUST_LOG=debug xcrun devicectl device process launch --device <UDID> <id>

# macOS: hand the same link to a running instance via the Apple Event path
# (`open 'po://…'` is unreliable — the simulator build claims the same bundle id)
osascript -e 'tell application id "com.debanjanbasu.po" to open location "po://pair?data=<payload>"'
/Applications/po.app/Contents/MacOS/po        # installed app, stdout attached

# Android
adb shell am start -a android.intent.action.VIEW -d "po://pair?data=<payload>" com.debanjanbasu.po
adb shell input tap <x> <y> && adb shell input text '<pin-or-message>'
```

`--payload-url` and `-e` are the reason the same test vocabulary works on three
platforms: a scenario is a URL plus an env dictionary.

**2. Observe — one log stream per device, one marker vocabulary.** `devicectl --console`
bridges the app's stdout; `adb logcat -v time RustStdoutStderr:D '*:S'` is the Android
equivalent; `dx serve`/the raw binary is the Mac's. Assertions grep that stream, so the
assertion surface is identical across platforms.

The app's own `tracing` domains remain the triage vocabulary
(`auth.pairing`, `sync.queue`, `user.identity_watch`, `identity.watcher`, `user.record`),
but assertions do not read prose: with the seam on, every milestone a scenario asserts also
writes one line on the dedicated `e2e` target — `src/config.rs::e2e` owns the flag, its call
sites own the lines.

| Marker | Fields | Meaning |
|---|---|---|
| `app_started` | `platform` | the first line a native peer emits, which is what `Surface::ready` waits on: those surfaces match `/event=/`, so this one is load-bearing for every native scenario; `smoke` asserts this marker by name rather than trusting that wait |
| `share` | `pin`, `deep_link` | the share screen produced this account's pairing bundle |
| `deep_link` | `accepted` | a launch URL was consumed by the app — the only proof the link reached the peer being watched, since the env seams can complete a pairing without it (`src/platform/deep_link/mod.rs`) |
| `qr` / `qr_decode_failed` | `link` / `reason` (`mismatch`\|`undecodable`) | the rendered QR decoded back to the exact share URL, or did not |
| `identity` | `username`, `dht_key` | this device rendered its own contact QR — the only source of its own identity on native |
| `invite_sent` | `conversation` | the conversation-invite op was queued for the peer |
| `invite_received` | `from`, `conversation` | a peer's invite was PROCESSED and the roster was hydrated enough to name the sender. An invite from an unknown sender parks first (`src/chat/invite/receiver.rs`) and only emits this once the user accepts it — so it is not the marker for a parked row (S14 waits on the Dismiss control instead) |
| `conversation_active` | `conversation`, `peer` | the contact flipped to Active — the conversation is usable |
| `message_sent` | `conversation`, `msg_id` | the message left this device (feed op queued) |
| `message_delivered` | `conversation`, `msg_id` | delivery to the peer was confirmed |
| `message_received` | `conversation`, `msg_id`, `from` | a peer's message was newly hydrated locally |
| `deep_link_send` | `contact`, `conversation`, `accepted`, `reason` | the harness-only `po://send?…` action ran the app's ordinary send path with no conversation view open (`accepted=true` means the message was constructed and queued). A refusal is emitted too, naming its `reason` (`no_session`, `no_conversation`, `no_key`, `not_in_roster`, `too_long`, `canceled`, `not_queued`, `save_failed`), so a scenario sees the cause instead of a timeout |
| `pairing_rejected` | `reason` (`expired`\|`invalid_credentials`\|`cooldown`\|`superseded`) | the pairing attempt was refused, with its kind |
| `queue_drained` | `pending`, `dead` | the pending-op queue emptied; no ready op is left |
| `dead_lettered` | `op`, `attempts` | an op exhausted its retries and moved to the DLQ |
| `dht_ready` / `dht_ready_timeout` | `waited_ms` / `budget_ms` | Veilid's own DHT gate opened, or the wait expired. **Joining path only** (see § Readiness) — a peer that merely signs in never emits it |
| `paired` | — | the joiner's pairing sync reached Complete |
| `watch_reconciled` | `record` (`profile`\|`security`\|`app_state`\|`contacts`\|`conversations`\|`totp`) | a watched record's network copy merged into local state |
| `profile_merged` | `field` (`dob`\|`terms_accepted`) | a merged profile field changed locally |
| `dict_synced` | `digest` (`b3:<hex>`) | this device adopted the shared compression dictionary |

Marker **names are emitted quoted** (`event="qr"`) while field values are not
(`conversation=…`), so a matcher written as `event=qr` misses every line;
`test/lib/wait.ts::findMarker` accepts both. Keep the vocabulary to what a scenario
asserts, and never log payloads, PINs or passwords — the share marker's `pin`/`deep_link`
and the pairing share URL are masked on the way into evidence
(`test/lib/run.ts::maskSecrets`). Mask the assembled line, not the bare value:
the masker's rules are prefix-anchored, so a raw PIN or URL passed on its own
survives.

Old evidence bundles may contain a `tmp_*` family (`tmp_route_*`, `tmp_conv_*`,
`tmp_signal_*`, `tmp_add_contact_gate`): about two dozen call sites across
`src/chat/signal/`, `src/chat/session_flow/` and `src/sync/` that only ever existed to
debug the route work. No scenario asserted them and nothing documented them, so they were
removed. The failures they were watching still log — the route files keep their own
`debug!`/`warn!` lines, and `publish_with_retry` reports every publish failure with its
attempt number, delay and error chain. What is gone is the per-call success summary
(route-load entry counts, cache-hit versus DHT resolve): that ran on every message, and
`RUST_LOG=debug` is where a route question is answered now, not the marker channel.

### The web marker channel

On wasm `std::env::var` is a stub, so `PO_E2E=1` cannot reach the app. The web surface sets
**`localStorage.po_e2e = "1"`** instead (before any page script, via Playwright's
`addInitScript`, and re-asserted after navigation), and `src/config.rs::e2e` reads it at
event time. It is storage rather than a query parameter because the app evaluates the flag
when it emits, and the app's own in-app navigation drops the query string.

**3. Capture — artifacts for the human:**

```bash
xcrun devicectl device capture screenshot --device <UDID> --destination runs/<ts>/iphone.png
adb exec-out screencap -p > runs/<ts>/android.png
screencapture -o -l "$(osascript -e 'tell app "po" to id of window 1')" runs/<ts>/mac.png   # [INFERENCE] window-id form
```

For anything the scripts cannot assert (visual state, the Activity modal), screenshot
both devices before restarting anything — restarts clear in-memory error history
([docs/debugging.md](debugging.md#in-app-the-activity-panel-first-stop-all-platforms)).

## Tandem scenario catalog

Registered in `test/scenarios/registry.ts`; `node test/run.ts --list` prints the same names
plus the declined list. Each scenario names its observable, not just its steps. Owner docs in
brackets.

| # | Scenario | Observable |
|---|---|---|
| smoke | launch, assert the first screen, screenshot each peer | the process is alive and reached a first screen; the surface-level sanity check |
| S1 | A signs in (anchor), B cold + signed out, `po://pair?data=…` on B + PIN + password | `PAIRING_SYNC_STATUS` reaches ready on both; B opens the identity record and child records hydrate; no `auth.pairing` background error. The joiner's `deep_link` marker is asserted **before** the `paired` wait, because `paired` can pass on the env seams alone [dht-architecture, watch-system] |
| S2 | B adds A via `po://chat?data=…` | `ConvRecordCreate` drains, invite signal arrives, both sides show the conversation Active [peer-signaling, sync-pipeline] |
| S3 | A→B and B→A message, with a typing/read exchange between | sender moves queued→delivered without DLQ; receiver renders the bubble and emits `message_received` [peer-signaling] |
| S4 | B edits profile (DOB); A holds an unsaved conflicting edit | A merges via `user.identity_watch` and shows the DOB-conflict warning row [watch-system] |
| S5 | B force-quit/airplane-mode mid-send, then relaunch | queue drains once, receiver sees one message (pending-op dedupe), no tombstone replay [sync-pipeline] |
| S7 | A trains/upgrades the zstd dictionary | B receives it via `dict_sync` and both report the same `b3:<digest>` [compression] |
| S8 | Kill B while a thread is open, relaunch | archive/backfill recovers history, watches reconcile — pinned to `watch_reconciled record=app_state`, since the marker name alone is satisfied by any of five record sources [watch-system] |
| S9 | Negatives: expired payload TTL, wrong PIN (cooldown path) | each shows its own UI error/notice and writes no partial admission state |
| S10 | TOTP: add a known base32 secret on `/totp` | the rendered code equals an independently computed RFC 6238 TOTP (HMAC-SHA1, 30 s, 6 digits); deleting the entry removes it |
| S11 | Security: change the account password; try a wrong PIN | the new password admits, the old is refused, a wrong PIN is refused at the gate |
| S12 | Snapshots: export a snapshot, then import/restore it | the restored state matches what was exported — the backup path, which fails silently |
| S13 | Session: switch theme, reload; then log out and reload | the theme survives the reload; logout returns to login and a reload does not restore the session |
| S14 | A parked invite from an unknown sender is dismissed — the decline half of the invite journey (S2 only accepts) | the parked-invite row itself — the `Dismiss` control on it — clears it, the receiver's chat list still reads "No contacts yet", and `conversation_active` never fires on either peer (counted against a pre-click baseline, after a settle window). Deliberately **not** `invite_received`: a parked invite never emits it (see the marker table), so waiting on that marker made this scenario fail exactly when the app behaved correctly |
| S15 | B types into its composer without sending, after one seed message | the peer renders exactly one typing bubble (`#chat-messages .loading-dots`), the typist's own view renders none, the bubble is gone by the end of the app's 5 s typing expiry, and the typist's transcript still holds only the seed message |
| S16 | Import the SAME backup twice on one device: Merge, then Replace | Merge keeps a token added after the export; Replace discards it, waited for as `#totp-empty-state` — so neither "the restore did nothing" nor "Replace is a Merge" can pass, and an instant read cannot mistake a restore that is still applying for one that did nothing. Its post-export change is a TOTP token, and the seed must therefore be ≥128 bits: the screen parses through `totp_rs` (`src/components/features/totp.rs`), which refuses the 80-bit RFC 4226 example with its own validation error, so it uses S10's RFC 6238 Appendix B secret |
| QR | the share QR round-trips | the rendered QR decodes back to the exact share URL handed to the joiner (`qr` / `qr_decode_failed`) |
| S17 | Pair a device, then message it: the joiner pairs from the pairing alone (S1's flow), and the DOM surface's second context holds the counterpart account the conversation is with (`minPeers: 3` — the first surface takes the extra peer, `test/run.ts::planOwnership`) | `message_sent`, `message_delivered` and `queue_drained` on the counterpart, `message_received` naming the same `msg_id` on the PAIRED device — the messaging path S3 cannot reach on a surface with no typing channel. The paired device sends the invite itself (a contact deep link is its only outbound chat action: the adder's path needs no accept click, and a device with no DOM cannot press Accept), and the return direction is driven by `po://send?…` where the joiner has no typing channel — the same assertions as the DOM joiner's, refused-or-accepted by `deep_link_send` — recording a SKIP only for a surface with neither channel |
| S18 | Launch on the local network (native surfaces only) | `event=net_locality` with `local_network_ready=true` — the node reached its LocalNetwork routing domain — and the count of recently-accessed peers sitting on a LAN address (`local_peers`), inferred from the peer table's observed addresses: the API exposes no per-peer routing domain, transport or route kind, so this is the strongest locality claim the app can honestly make |

**S10–S13 and S16 are single-peer feature flows**, not tandem scenarios. They need no
second device and drive the app through the generic `dom` op (`test/lib/types.ts::DomOp`),
which exists so a new feature scenario does not have to add a purpose-built method to the
shared web surface. Run them with `--surfaces web`. S14 and S15 *are* tandem (two peers) and
assert DOM-side on both, so they run on the same single `web` surface — its two isolated
contexts are the two peers (`pickPeers` falls back to `peerIds[1]` when only one surface is
listed).

Cases deliberately **not** automated, all reported by `--list`:

* **S6** — Android denying `ACCESS_LOCAL_NETWORK`. The *toggle* exists —
  `adb shell pm revoke com.debanjanbasu.po android.permission.ACCESS_LOCAL_NETWORK` — but
  the app never reports the outcome: the Rust JNI entry point checks the permission and
  calls `requestPermissions`, then starts Veilid without waiting for or logging the
  answer, so there is no marker for a scenario to assert on. Automating S6 needs that
  marker first (and, because the request is asynchronous, that Veilid is told about the
  grant afterwards rather than only at the next launch).
* **S9 case 3** — a pairing link opened on an already signed-in device cannot be driven:
  a browser has no launch-URL source, and on native the refusal is a toast with no marker.
* **Biometric gate and toggle** — the OS owns the prompt; neither the browser nor `simctl`
  can script an accept/deny result.
* **Camera QR *scanning*** — needs a live camera feed. `QR` covers the render and decode
  round trip, not the scan path.
* **Share sheet** — a platform UI outside the DOM, with no observable on any surface.

`S12` does assert a **real file round trip**: its export is captured as an actual browser
download and that same file is fed back through the import input (`dom`'s `download` and
`setFiles` ops). Its first import attempt deliberately uses a wrong password, so "the
import succeeded" cannot also be true of an unencrypted dump.

Run order matters: S1 → S2 → S3 prove the happy path; S4, S5, S7 and S8 are the
cross-device CRDT/sync/compression behaviours worth automating early because they regress
silently; S9 and QR pin down the negative and round-trip edges; S10–S13 cover the
single-device feature surface (authenticator, credentials, backups, session) that the
tandem scenarios never touch; S14–S16 close the feature-side gaps an audit found — the
invite decline path, the typing signal, and the destructive half of the restore; S17
extends messaging past S3's `signIn`-on-both-peers gate, to a device that has no typing
channel at all (run it as `--surfaces web,ios-device`, where its first surface hosts the
anchor and the counterpart and the phone is the joiner).

**A skip is not a pass.** `support.ts::skip` records a `skip` timeline entry rather than
throwing, because a surface legitimately cannot run some steps (a native peer has no
`signIn`, so S3 skips there). `result.json` therefore carries `skipped` — the capability
names the run skipped — alongside `ok`, and `run.log` prints
`result: PASS — NOT coverage: N skipped (…), those steps asserted nothing`. Reading `ok`
alone counts a scenario that asserted nothing as coverage; reading `skipped` does not.

**Concurrent runs are not safe.** A run rebuilds the `dx` bundle when the tree is stale and
serves it from disk, so two runs on one tree compile into the same target directory and can
serve each other half-written files — a flaky scenario failure rather than a build error.
Authoring can be parallel; running goes through `test/run-locked.sh`, which serializes on a
lock directory (`$TMPDIR/po-e2e.lock`, `LOCK_WAIT` seconds, default 900) and resolves the
repo root from its own location so it works from any cwd.

### The `dom` op — the generic driver for feature screens

`test/lib/types.ts::DomOp` is a small op list (`click`, `fill`, `waitFor`, `readText`,
`readCount`, `reload`, `download`, `setFiles`) executed by `test/surfaces/web.ts::dom`. It
exists so a **new feature scenario adds no method to the shared surface**: S10–S16 drive
their screens through it (S14 and S15 only their DOM-facing half), and only the ops
themselves live in the shared files.

Two habits this makes possible, and that the feature scenarios rely on:

* **Wait on something that is actually visible.** A control inside a collapsed `<details>`
  (S12's "Create Snapshot" sits behind "Snapshot History") resolves in the DOM with no box,
  so a `waitFor` on it times out while looking like a missing element. Wait on the
  disclosure's `summary` first.
* **Let the app's own effects drive the flow.** S12's import modal has no submit button: a
  `use_effect` decrypts as soon as the typed password validates, and disables the field
  while it runs. A keypress on top of that races the effect and fails on a disabled input.

Reach for a new op only when a screen genuinely needs a new capability (S12's `download`
and `setFiles` are the file round trip); do not add a per-feature surface method.

## Device pairs and the inner loop

| Pair | Use | Cost |
|---|---|---|
| Web anchor ↔ native joiner | **the harness's working pair** — the web surface signs in and shares (it is the only surface with a DOM to type the account password into); the native surface joins from its launch environment. S1 stops at the pairing; S17 goes on to put the joined device in a real conversation and message it | **available now** — `node test/run.ts --scenario S1 --surfaces web,macos,ios-sim`, and `--scenario S17 --surfaces web,ios-device` for messaging on the joined device |
| Mac ↔ Mac (two native instances) | ideal inner loop: real UDP LAN, no hardware, scriptable | available — `PO_INSTANCE=<name>` isolates the data dir and the Veilid namespace (`src/veilid/config.rs`, `src/veilid/core/mod.rs`). Needs a UI driver on the anchor side to sign in |
| Mac ↔ iPhone | cheapest real pair; Mac side needs no driver (`open` + stdout) | needs xtool + unlocked/trusted phone |
| Mac ↔ Android | most automatable real pair | needs Android SDK/`adb` |
| iPhone ↔ Android | pre-release gate, no Mac app involved | both device constraints at once |

The harness's shape follows from one asymmetry: only the web surface can type the account
password, so it must be the anchor; every native surface can take the pairing payload,
PIN and password from its launch environment, so any of them can be the joiner. That
also means the *joiner* is what gets real-hardware coverage — the platform code under
test is the one that receives the pairing, not the one that produces it.

**Harness seams — shipped, and off unless asked for** (`src/config.rs::e2e` compiles every
accessor to `false`/`None` unless the build has `debug_assertions` **or** the `e2e-seams`
cargo feature; a distributed release has neither):

| Knob | Effect | Where |
|---|---|---|
| `PO_INSTANCE=<name>` | Veilid data dir becomes `.veilid-<name>` and the namespace `com.debanjanbasu.po-<name>`, so two instances coexist on one machine (Mac↔Mac tandem). A per-run name (the phone and the emulator use one) creates one data dir per run — the app prunes the stale siblings, see [the instance-dir bound](#the-instance-dir-bound) | `src/veilid/config.rs`, `src/veilid/core/mod.rs` |
| `PO_E2E=1` | turns on the whole `e2e` marker channel (the marker table under [Observe](#the-three-channels)); the share screen then logs `event=share pin=<12 digits> deep_link=<po://pair?…>`, so the PIN comes from stdout instead of a UI scrape. On wasm the same flag is read from `localStorage.po_e2e` | `src/config.rs`, call sites across `src/` |
| `PO_E2E_PAIR_PIN` + `PO_E2E_PAIR_PASSWORD` | the joining side prefills those two fields and rides the screen's ordinary auto-submit — same validation, same admission path, nothing bypassed, values never logged | `src/components/auth/pairing/pair_mode.rs` |
| `PO_E2E_PAIR_PAYLOAD` | the pairing payload itself (deep link or raw base64url), so a joiner needs no deep-link delivery either — the whole form can be filled from the environment | `src/components/auth/pairing/pair_mode.rs` |
| `PO_E2E_ACCOUNT_PIN` | the account's second factor, so an unattended device clears the PIN gate that follows admission | `src/components/auth/pin_gate.rs` |
| `PO_E2E_PAIRING_TTL_SECS=<n>` | the pairing payload's lifetime, replacing the production 300 s. Read by the **creating** side only (a payload carries its own `expires_at`), and by the share countdown and beacon window so the app stays self-consistent; S9 uses it to take the real expiry path in 15 s instead of a five-minute static sleep (which was 349 s of the web sweep's 799 s). Never settable in a distributed build — outside a seam build the accessor is the constant | `src/config.rs::e2e`, `src/user/pairing_crypto.rs`, `src/user/pairing_beacon.rs`, `src/components/auth/pairing/share_mode.rs` |

The joiner needs no UI driver where env injection exists: `SIMCTL_CHILD_*`, `DEVICECTL_CHILD_*`
or a plain shell.

**Android has no environment channel, so it uses intent extras.** `adb shell am start`
cannot set environment variables for the app it starts, so the harness passes the same
seam values as intent extras named `po_e2e_<KEY>`:

```bash
adb shell am start -n com.debanjanbasu.po/dev.dioxus.main.MainActivity \
  --es po_e2e_PO_E2E 1 --es po_e2e_PO_INSTANCE b-20261105T101500
```

`MainActivity.onCreate` reads the extras, strips the `po_e2e_` prefix, and hands the
key/value pairs to Rust over JNI (`cacheE2eSeams`); `config::e2e::seam_value` then
reads the environment first and the intent extras second, so **the Rust-side seam
names are identical on every platform** — `PO_E2E`, `PO_INSTANCE`, `PO_E2E_PAIR_*`.
A fresh launch always hands the seam set over — including an empty one — so a
launch without `po_e2e_*` extras cannot inherit the previous run's seams. A
deep-link intent arriving mid-session (`onNewIntent`) never touches the seams.
The app's own logcat tag is `RustStdoutStderr`, which is where the `event=` markers
land.

Two Android-specific facts the surface depends on:

* **Install the release APK.** `dx build --release --platform android` emits the
  gradle `release` variant only when `[bundle.android]` configures signing; without
  that section it falls back to a `debug` variant whose `libmain.so` is unpacked and
  unstripped (**952 MB** uncompressed against the release build's **28 MB**), which
  does not fit on the emulator's `/data` alongside anything else. The release
  workflow appends that section at run time from the `release` environment's secrets
  and never commits it, so CI's APK is R8-minified and release-signed — measured
  15.5 MB against the debug variant's 42.2 MB, one `classes.dex` of 1.5 MB against
  eight files totalling 14.6 MB.
* **Grant `ACCESS_LOCAL_NETWORK` before launching.** Without it the activity blocks
  on the permission dialog and the app never starts — the log then shows the Veilid
  banner and nothing else, with no `event=` line at all.

Marker lines are emitted through one macro, `config::e2e::event!`:

```rust
config::e2e::event!("conversation_active", conversation = %manifest_key_str, peer = %contact.username);
```

It expands to `if e2e::enabled() { info!(target: "e2e", event = …) }` — the
`enabled()` check compiles to `false` outside a seam build, so a shipped binary
emits nothing. Use it for every marker; reach for a raw `e2e::enabled()` guard
only when the guard is doing something **other** than emitting a marker (the QR
round-trip check, the pairing retry budget, the delivery-status report).

The **anchor** still needs a typing channel: signing in and opening the share screen both
require the account password to be entered. Only the web surface has a DOM to type into,
so in the harness the web surface is the anchor and the native surfaces are joiners.

### The instance-dir bound

The phone and the emulator give each run its own `PO_INSTANCE` (derived from the run
directory's timestamp), because both keep the app's data between runs and a remembered
session would restore straight into the signed-in home screen — the pairing form would
never appear and the pairing link would be correctly refused. The simulator avoids this
by uninstalling the app.

A per-run instance name means one data dir per run, and the phone's app container is not
reachable from the host — the harness cannot clean it up. So **the app prunes its own
siblings** (`src/veilid/config.rs::prune_stale_instances`), at startup, in seam builds
only:

* a sibling `.veilid-*` dir older than **24 hours** is removed;
* and at most **4** siblings survive, so the count is bounded no matter how many runs
  happen — the oldest go first.

A dir younger than the window is another live instance's (two instances can run side by
side), so the age rule leaves it alone; the ceiling rule only removes the oldest, and
only beyond the limit. The prune is best-effort: a failure to remove a dir never stops
the app from starting. Verified on the emulator: 8 stale dirs plus the live one came
back as 4 siblings and the current dir.

### Per-surface capability matrix

`test/lib/types.ts::Surface` is the contract; everything past `openUrl` is optional, and
scenarios probe for a method before using it (`test/scenarios/support.ts::pickPeers`,
`skip`) rather than assuming it.

| Surface | Optional methods it implements | Marker channel |
|---|---|---|
| `web` | the DOM-driven set — `signIn`, `openShareScreen`, `readShare`, `openPair`, `waitComplete`, `identity`, `addContact`, `acceptInvite`, `openConversation`, `sendMessage`, `readMessages`, `editProfile`, `stageProfileEdit`, `readProfileConflict`, `readText`, `openPairing`, `queryDom`, and the generic `dom` op | `localStorage.po_e2e` |
| `macos` | `identity`, `readShare`, `restart`, `relaunch` | `PO_E2E=1` |
| `ios-sim` | `identity`, `readShare`, `restart`, `relaunch` | `SIMCTL_CHILD_PO_E2E=1` |
| `ios-device` | `identity`, `readShare`, `restart`, `relaunch` | `DEVICECTL_CHILD_PO_E2E=1` |
| `android` | `identity`, `restart`, `relaunch`, `queryDom` (CDP over adb) | `po_e2e_*` intent extras |

The native surfaces expose `identity`, `readShare`, `restart` and `relaunch` and nothing
else; every other optional method would need a DOM. `readShare` is the one that does not:
it reads the `share` marker's `pin`/`deep_link` fields, so a native peer can supply the
pairing bundle without a share screen to scrape. (No scenario drives it yet: reaching the
share screen still needs a typed account password, so the anchor stays `web`.) Android
reaches the marker channel through the intent extras described above, so it reads `event=`
lines like the other native surfaces.

**Web tandem caveat:** two Chromium contexts on one machine (Playwright, isolated
storage) exercise pairing/messaging UI logic and CRDT merge cheaply, but WASM Veilid has
no UDP/local discovery — web peers reach the network only through ws/wss/WebRTC. Treat
web tandem as UI-flow coverage, never as evidence for the native LAN path. [INFERENCE on
the exact WASM transport mix in this fork; confirm with `RUST_LOG=veilid_core=debug`.]

### Does "same network" mean the peers find each other?

**There is no local-network discovery in this fork.** `veilid-core` (rev `ac298e9`)
contains no mDNS/Bonjour implementation — the service strings appear nowhere in it. The
plists' `NSBonjourServices` (`_veilid._udp`, `_veilid._tcp`) only *permit* such services;
nothing advertises or browses them. Peers still reach each other over a LAN, but by a
different mechanism:

* **LAN routing is address-based, not discovery-based.** `RoutingDomain::LocalNetwork` is
  declared first in the enum — "listed in order of preference" — and its detail object
  keeps only interface addresses that classify as local
  (`local_network/detail.rs::set_interface_addresses`, filtered by `Address::is_local()`
  = RFC 1918 / link-local / ULA; the two domains are explicitly kept disjoint). A peer
  whose published dial info is a private address is therefore dialled in the local domain.
* **Finding the peer still goes through the DHT.** A peer's dial info is published there,
  so both peers must be attached to the public internet before either can learn the
  other's local address. **LAN-only operation is impossible**, and not for want of
  configuration: the storage manager returns early for every domain except
  `PublicInternet` (`storage_manager/mod.rs::routing_domain_ready_event_handler`), so
  `dht_is_online` — and with it every record open — cannot be satisfied by local-network
  readiness. `NetworkStatus::dht_ready()` documents the same thing from the app side.
* **The two permissions are still load-bearing** for the local *path*: iOS needs the Local
  Network grant, Android needs `ACCESS_LOCAL_NETWORK`. Android's is requested from Rust
  (`platform/native_bridge.rs::Java_dev_dioxus_main_MainActivity_initVeilidAndroid` checks
  it and calls `requestPermissions`); `veilid_core_setup_android` runs unconditionally
  afterwards, so a denial cannot gate internet attachment — the invariant AGENTS.md
  states. Nothing reports the outcome to the harness, so no scenario can assert it yet.
  **Suspected defect, unverified (no device attached):** because `requestPermissions` is
  asynchronous and Veilid is initialised in the same call, the first launch after install
  starts with no local-network access, and Veilid's address-change detection is not told
  about the later grant. If it does not recover on its own, the LAN path is missing for
  that entire first session — which is exactly the "two devices on one Wi-Fi never go
  direct" report this page would otherwise not explain. Check
  `RUST_LOG=veilid_core=debug` for a `LocalNetwork` inbound stage on a freshly installed
  build and relaunch once before concluding anything.
* **`local_network_ready` is not a peer signal.** It is computed from this node's own
  local-address and published-peer-info state, so it reads true on a lone machine with no
  LAN peer anywhere. Do not use it to assert that two devices found each other.

* **The locality seam (`event=net_locality`) says exactly this, and nothing more.**
  `src/veilid/core/updates.rs::locality_seam` emits the domain readiness and, per peer in
  the `Network` update's table, whether the *observed address* sits on a LAN. The API gives
  the app no per-peer routing domain, transport or route kind — `VeilidStateNetwork`
  carries only `started`/`bps_*`/`peers`/`node_ids`, `VeilidRouteChange` only dead route
  ids — so `local_peers` is inferred from each `peer_address` string (`PROTO:<socket
  addr>`), which is the *last observed* remote address and counts a peer through carrier
  NAT (100.64/10) as remote. A zero is an observation, not a failure: a node that has not
  met a LAN peer has only internet bootstraps in its table (S18 records it that way). It
  still cannot prove two devices found each other — for that, nothing beats seeing a
  message land (S17). Gate note: the seam is compiled into any `debug_assertions` build,
  so a plain `cargo run` carries it; the marker emits only under `PO_E2E=1`, and the
  release path compiles the whole module away.

* **Android emulator** — the AVD is NAT'd (10.0.2.x), so the emulator's interface is not a
  private address of the host's LAN; expect the **internet path**, and treat any local
  verdict as meaningless.
* **iOS simulator** — shares the host's network stack, so it behaves like a second process
  on the Mac. Whether the fork's `LocalNetwork` domain ever carries traffic between
  same-host peers (loopback) is **unverified** — measure it before relying on it. Note that
  the simulator does not implement local-network *privacy* at all (see below), so a
  permission-shaped failure cannot be reproduced there.
* **Web (WASM)** — no UDP at all: bootstrap / WebSocket / WebRTC only.

#### Granting local-network access per platform

Per Apple's TN3179 (*Understanding local network privacy*), the authority for all of the
below:

| Surface | How the grant works | What the harness can do |
|---|---|---|
| iOS simulator | **Not applicable** — "the simulator doesn't support local network privacy" | Nothing to grant, and nothing to conclude: the simulator is not evidence about the local-network path either way |
| iOS device | A real user prompt on the first local-network *operation* (outgoing TCP to a local address, UDP unicast send, connecting a UDP socket, any Bonjour operation). No programmatic grant exists, and MDM cannot configure it | One-time manual grant. Reset to undetermined by **deleting the app**, or Settings → General → Transfer or Reset → Reset Location & Privacy |
| macOS app | The prompt is per app, but macOS auto-allows `launchd` daemons, anything running as root, and **command-line tools run from Terminal or SSH plus their children** | Free: `test/surfaces/macos.ts` spawns the binary directly, so the peer is a CLI child and is auto-allowed. A user who double-clicks the `.app` is not, and will see the prompt — so a LAN bug that reproduces outside the harness but not inside it is this, not Veilid |
| Android | `ACCESS_LOCAL_NETWORK` is a runtime permission: `adb shell pm grant <pkg> android.permission.ACCESS_LOCAL_NETWORK` / `pm revoke`, or `adb install -g` to grant every runtime permission at install | Both directions are scriptable, which is what S6 would use |

Two details worth knowing before chasing a LAN defect:

* On iOS, multicast and broadcast additionally need the
  `com.apple.developer.networking.multicast` entitlement; plain UDP unicast (Veilid's case)
  does not. `NSBonjourServices` is likewise only for Bonjour registration/browsing, so with
  no mDNS in `veilid-core` the app's `_veilid._udp`/`_veilid._tcp` entries are inert rather
  than load-bearing.
* There is **no API to read the local-network privilege state** (Apple's FB8711182). Apple's
  two indirect probes: for Bonjour, look for `kDNSServiceErr_PolicyDenied` (-65570); for a
  TCP connection managed by `NWConnection`, look for
  `currentPath?.unsatisfiedReason == .localNetworkDenied`. Veilid uses neither API, so the
  app has no way to report the state either — which is why there is no marker for it.
  The nearest app-level proxy is `event=net_locality local_network_ready=true` (S18): a
  grant is what lets the LocalNetwork domain reach "ready", and a denial keeps it
  unsatisfiable. It reads the *domain*, not the privilege, so treat it as evidence that
  the node can see its own LAN, not as a permission read-back.

For a CI macOS runner the escape hatch is documented by Apple as being for exactly this
use: `sudo defaults write com.apple.network.local-network AllowedWiFiLocalNetworkAddresses
-array "<cidr>"` (and `AllowedEthernetLocalNetworkAddresses`), which makes every address on
that network non-local for every program regardless of privilege. It needs a restart. It is
**not** wired into `e2e.yml`, because the harness's spawned-binary path is already
auto-allowed.

Two consequences for scenarios: (1) pairing and sync do **not** depend on a LAN path —
the joiner pulls the anchor's identity record from the DHT, so what matters is that both
peers are attached and the anchor's record is published; assert eventual convergence with
generous timeouts rather than asserting the transport. (2) Record which path was used
(`RUST_LOG=veilid_core=debug`) in the evidence bundle, so a slow run is diagnosable.

**Not verified:** that two real devices on one LAN actually complete a session — no
device is attached to this machine, and the harness has no native anchor (only the web
surface can sign in and open the share screen), so the pair is always web ↔ native and
the web side has no UDP at all. The S18 run on this Mac
(`test/runs/2026-11-11T105227Z-S18-macos/`) is a single host: its node reached its own
LocalNetwork domain (`event=net_locality local_network_ready=true … peers=0
local_peers=0`), which is the lone-machine case the bullet above warns about, not a
two-device proof.

### Readiness is the app's DHT gate, not the process starting

The app waits on Veilid's own `public_internet_ready` attachment condition before its first
record open (`src/veilid/core/mod.rs::wait_for_dht_readiness`) — the level-based signal the
storage manager gates record opens on. Measured **3–16 s after launch** on this hardware,
logged as `event=dht_ready waited_ms=…`, or `event=dht_ready_timeout budget_ms=…` when the
wait lapses.

Nothing a scenario asserts can happen before that gate opens, which is why
`test/scenarios/support.ts::BUDGET` is **120 s** and not a few seconds. A joiner can
therefore sit on its "Connecting" stage for ~15 s after its window first appears; that is
the gate, not a wedge. Assert on the marker, never on a UI stage name —
`test/scenarios/support.ts::assertDhtReady` is that assertion, and it checks both halves
(`dht_ready` present, `dht_ready_timeout` absent) so a lapsed gate names itself instead of
surfacing as a downstream timeout.

It **only applies to the joining path**, because that is the only path that runs
`wait_for_dht_readiness`: asserting it on a peer that merely signs in can only time out
while the app is perfectly healthy (the log shows `Veilid network attached successfully`
and `PublicInternet ready`). The callers are therefore exactly the joiners — S1's joiner
(via `support.ts::joinPairedDevice`'s caller), S4's paired device, asserted right after
`pairSecondDevice` returns, and S17's joiner. Everywhere a peer just signs in
(`smoke`, both peers of S3/S5/S8, S9's and QR's anchors, S4's alpha) the gate is covered
transitively instead: nothing those scenarios assert can happen before it opens.

## Simulators and emulators (in scope; hardware is the release gate)

Already available on this Mac, no install needed: iOS 27.2 simulator runtimes with
iPhone 18 Pro / iPhone Air / iPads, and an Android AVD (`Medium_Phone`) on the
`android-37.2` system image. `simctl` speaks the same three channels as `devicectl`
(`install`, `launch --console`, `openurl`, `io … screenshot`, `pbcopy`, `privacy grant`,
`erase`), so an adapter is a per-pair row, not a new harness. The simulator surface
(`test/surfaces/ios-sim.ts`) and the physical-device surface
(`test/surfaces/ios-device.ts`) are both in the tree and share the env-seam contract:
`SIMCTL_CHILD_*` and `DEVICECTL_CHILD_*` respectively.

Simulators and emulators are the default surface set for the harness — they need no
hardware, they reset deterministically (`simctl erase`, AVD wipe), and every channel the
harness uses (`simctl`, `adb`) is the same one the physical devices need. They are **not**
a substitute for hardware where the platform itself is the point: the planned AI surface
(`Ai`, `Calendar`, `Calls` routes are placeholders today) is hardware-bound — Apple's
on-device Foundation Models model is not available in the simulator and needs Apple
Intelligence-capable hardware, Gemini Nano runs on real devices through AICore — plus
cameras (QR) do not exist in the simulator, biometrics are simulated, and the emulator is
NAT'd. Hardware runs therefore stay the release gate for those flows.

Per-surface limits to remember: no camera in the simulator (QR) — use the paste/upload
intake path, or the deep link — simulated biometrics only, the emulator's NAT'd network
(see above), and `adb shell pm grant com.debanjanbasu.po android.permission.ACCESS_LOCAL_NETWORK`
if a scenario needs the LAN grant without a prompt. Android needs no typing
channel at all: its seams travel in the launch intent's `po_e2e_*` extras (the intent-extras
paragraph under [The three channels](#the-three-channels)), so the joiner form prefills from
them like every other native surface, and `test/surfaces/android.ts` implements no
`adb shell input text` at all.

Two build-output gotchas when both an iOS simulator and an iPhone are in the same run:
dx writes the simulator and device bundles to the same `target/dx/po/debug/ios/`
directory (the device build wins and clobbers the simulator one), and `dx build
--platform ios --device` additionally demands an Xcode-managed signing identity that
this repo deliberately does not use. The device surface therefore builds into
`target/dx-e2e/` with `dx build --platform ios --target aarch64-apple-ios` (no
`--device`, so dx skips signing) and signs with xtool, matching
[`scripts/ios-device-deploy.sh`](../scripts/ios-device-deploy.sh).

### iPhone: USB for the install, Wi-Fi for the run

The device is pinned to the **iPhone SE (3rd generation)**
(UDID `00008110-001A44C63E10401E`); other phones paired to the same Mac are ignored.

The app must be signed by xtool — dx only signs the outer `.app`, and the Xcode-managed
identity it wants is exactly the one xtool revokes. xtool reaches devices through
usbmuxd, and on macOS 15+ the system usbmuxd has **no lockdown pair-record store**
(CoreDevice owns pairing): `ReadPairRecords` returns `BadCommand`, and `Connect` to a
Wi-Fi entry returns `BadDevice`. So `xtool devices --network` fails with
`LockdownClient.Error.muxError` even though the phone advertises
`_apple-mobdev2._tcp` and `devicectl` holds a live `localNetwork` tunnel.

Consequence: **the install step needs the USB cable; everything after it works over
Wi-Fi.** Launch, `DEVICECTL_CHILD_*` env injection, screenshots and the `--console`
log bridge all run through `devicectl`, which uses CoreDevice's tunnel. The surface
skips the install when the app is already on the phone, so after one USB install the
cable can stay unplugged for ordinary runs.

Confirmed working on the SE (2026-11-02): `xtool install --usb --udid <SE>` →
`XTL-NN5K5SW2NR.com.debanjanbasu.po` installed, `devicectl device process launch`
starts it, and the login screen renders (brand avatar, Username/Password, "Pair from
another device"). Two operational notes:

* **First install revokes xtool's own certificate.** The old XTL profile was
  `INVALID` (expired 30 Aug 2026) and referenced a certificate whose private key is
  gone from this Mac (`security find-identity` → 0 valid identities), so xtool cannot
  reuse it and prompts *"The following certificates must be revoked … Continue? (yes/no)"*.
  Answering `yes` revokes the **xtool-minted** cert (serial `6FD9A3CA…`, common name
  `Apple Development: Debanjan Basu (M453H85KW7)`) and mints a replacement
  (`4D253856…`) plus a fresh `ACTIVE` profile. This is the one-time flow
  [`scripts/ios-device-deploy.sh`](../scripts/ios-device-deploy.sh) already documents.
  It is a **portal-level** action: it invalidates signing with that certificate on any
  Mac using the same team — the other MacBook's Xcode-managed identity included.
  Keep xtool as the sole signer for this team.
* **The phone must be unlocked** before `devicectl device process launch` will start
  the app (`FBSOpenApplicationServiceErrorDomain error 1 / Locked`). The install
  succeeds on a locked phone; only the launch needs it unlocked. iPhone Mirroring
  cannot help here — it pauses the connection the moment the phone locks, so unlock
  on the device itself (or via Face ID) before a run.

## Phases

**P0 — toolchain + seams (shipped).** `scripts/dx-setup.ts` provisions the host toolchain
(macOS/Linux/Windows), and the seams above are in the tree, verified with
`cargo check`/`cargo clippy` for desktop and `cargo check` for web.

**P1 — harness (shipped; S1 green as of 2026-11-01).** Everything test-related lives in the
root `test/` folder: `test/run.ts` orchestrates, `test/surfaces/*` drive web (Playwright),
macOS, the iOS simulator (`simctl`), the physical iPhone (`xtool` + `devicectl`) and
Android (`adb`), `test/scenarios/*` hold the scenarios, and each run writes
`test/runs/<ts>-<SCENARIO>-<surfaces>/` evidence — one log per peer, screenshots, and
`result.json` with the marker timeline. The same entry point must work on a laptop and on a
GitHub runner, which is why no step may need a human. Verified 2026-11-01:

```bash
node test/run.ts --scenario S1 --surfaces web,macos,ios-sim   # PASS
node test/run.ts --scenario smoke --surfaces web              # PASS (2026-11-02, rebuilt the web bundle)
```

The anchor is always `web` (the only surface with a DOM to type the account password into);
`macos`, `ios-sim` and `ios-device` are joiners, which need no typing channel because the
payload, PIN and password arrive in the launch environment. `android` cannot anchor either —
it has no DOM to sign in through — and it takes its seams from the launch intent's
`po_e2e_*` extras rather than the environment, which is what turns its marker channel on
(see the [capability matrix](#per-surface-capability-matrix)). Per-surface details and the
physical-iPhone prerequisites are in [test/README.md](../test/README.md).

**P1a — physical iPhone (shipped; S1 green on real hardware).** `test/surfaces/ios-device.ts`
signs and installs through xtool and drives the app through `devicectl`. It is pinned to the
iPhone SE (3rd generation) and reaches it over `localNetwork` (tunnel connected, Developer
Mode on, phone unlocked); the **install** step needs the USB cable — see
[iPhone: USB for the install, Wi-Fi for the run](#iphone-usb-for-the-install-wi-fi-for-the-run).
The device build works unsigned: `dx build --platform ios --target aarch64-apple-ios`
produces a `platform IOS` bundle without needing an Xcode identity. Verified 2026-11-02 —
the iPhone paired with the web anchor and the run captured real on-device screenshots:

```bash
node test/run.ts --scenario S1 --surfaces web,macos,ios-device   # PASS
```

Two harness gaps that surfaced during that run are now handled: the surface refuses to
launch on a locked phone (it used to die silently and wait out the timeout), and it
discards an all-black screenshot — a locked phone's framebuffer captures as a valid but
empty PNG, and shipping that as evidence looks like a real capture.

The run also exposed an app-level bug the harness had been masking: see
[Cold-routing-table opens](#cold-routing-table-opens-tryagain) below.

### Cold-routing-table opens (`TryAgain`)

The first device run logged `Pairing failed: failed to open DHT record` on the iPhone —
and the same warning appeared in the simulator run, so it was app-level, not device
specific. The timing was consistent on both surfaces: the failure landed ~2 s *before*
"PublicInternet ready", i.e. while the node's DHT routing table was still cold.

Veilid reports that state as `TryAgain` ("no node answered the open"), explicitly
retryable, but the app treated it as terminal. The first pairing attempt therefore died
even though the network came up moments later — and the only thing that rescued it was
the harness's E2E-only retry seam, which a distributed build does not have. A real user's first
pairing attempt failed with an opaque internal-error message and needed a manual retry.

Fixed in `open_record()` (see [docs/dht-architecture.md](dht-architecture.md)), which now
retries `TryAgain` with a short backoff. The second half of the bug was visibility:
`anyhow`'s `Display` prints only the outermost context, so the log and the Activity modal
showed "failed to open DHT record" and never the cause that identified it. Both are
documented in their owning docs.

**The same gate exists on the value read, and cost S1 a second time.** The record open
retried `TryAgain`; `get_dht_value` did not. A physical iPhone whose post-pairing identity
pull ran ~3 s after launch hit the cold table on a *subkey read*, `prepare_pull()` returned
`None`, and the sync aborted before the `paired` marker — the app sat on the home screen
looking paired while nothing synced. `get_dht_value_with_retry()` now applies the same
budget to both subkey read paths; the full account is in
[docs/dht-architecture.md](dht-architecture.md#cold-routing-table-subkey-reads-tryagain).

The S1 failure shape is worth recognising: markers present, `paired` absent. That is a sync
that did not finish, **not** a seam gap — the seam shipped. `test/scenarios/S1.ts` says so
in its `SeamMissing` message rather than blaming a missing marker, and the real cause is in
the joiner's `WARN` lines (`Failed to pull identity from DHT: … TryAgain`).

**P2 — marker vocabulary (shipped).** The `e2e` target now carries every milestone the
scenarios assert; the marker table under [Observe](#the-three-channels) is the list and the
seam's `.rs` call sites are its source of truth. `test/lib/wait.ts::waitForSignal` matches a
marker only, records the line it matched on the run's timeline, and reports a signal that
never fires as `SeamMissing` when no `event=` line reached the log at all. Add a marker only
where a scenario proves a prose `INFO` line ambiguous — the seams are a means, not a
checklist.

**P3 — optional, only if scripted iOS taps become load-bearing.** Either iPhone
Mirroring + the agent's window automation (typing/clicks/screenshots, no signing
conflict) or a real XCUITest target (new Xcode project + a second signing identity kept
away from xtool's cert). Do not start here.

## Risks and gotchas

* **Signing collision.** Any tool that builds WebDriverAgent/XCTest runners mints
  Xcode-managed Apple Development certificates; xtool revokes those when it signs. Pick
  one automation stack that needs no Xcode signing (this doc's default), or isolate a
  second Apple ID/keychain for drivers.
* **Local-network permission.** iOS and Android gate LAN peers behind a runtime grant
  (`NSLocalNetworkUsageDescription` + `_veilid._udp/_tcp` are declared). Denial silently
  removes peers, so prime the grant once per install and treat S6 as a first-class
  scenario instead of an accident.
* **Dev-server coordinates do not reach physical devices** (script header): tandem runs
  must inject them explicitly (`--payload-url`/`-e` or `DEVICECTL_CHILD_*`) or run a
  non-hot-reload build.
* **Stale generated env.** New shells take `JAVA_HOME` from `~/.config/po/env.sh`
  (`$XDG_CONFIG_HOME`); a file written by an older revision of the script can pin a JDK
  the pinned Gradle refuses (outside the range it accepts, Gradle fails to
  start — *"Unsupported class file major version"*). Repair: `node scripts/dx-setup.ts
  --yes`, then confirm a fresh shell reports an in-window JDK.
* **`dx` shadowing.** `command -v dx` must resolve to `~/.cargo/bin/dx` (the revision
  `Cargo.lock` pins); a brew-installed `dx` earlier in `PATH` builds with a different CLI,
  and `dx-setup.ts` warns when it sees exactly that.
* **Bundle-id namespacing** on iOS (`XTL-<team>.com.debanjanbasu.po`) — always resolve
  the installed id via `devicectl device info apps` before launching; never hardcode.
* **macOS window restoration.** An uncleanly killed macOS instance left macOS holding a
  window-restore record, so the next launch stopped at a modal *"The last time you
  opened po, it unexpectedly quit while reopening windows. Do you want to try to
  reopen its windows again?"* before its first frame. That prompt belongs to the
  WindowServer, not the app: it is absent from the app's `AX` tree, from
  `~/Library/Saved Application State`, and from every user plist, and deleting those
  does not clear it. It is fatal for an unattended run because the harness kills and
  relaunches the app between scenarios. `native/apple/Info.macos.plist` now sets
  `NSQuitAlwaysKeepsWindows=false`, so macOS saves no window state and never offers to
  restore it — verified after both a clean quit and `killall -9`. If a machine still has
  a stale record from an older build, answer the prompt once (System Events can reach
  the button when `computer` cannot:
  `osascript -e 'tell application "System Events" to tell process "po" to click button "Don’t Reopen" of window 1'`).
* **Secrets in evidence.** Pairing links embed the PIN, and the P2 seam would carry a
  password. Mask link values in `result.json`/log excerpts; keep e2e accounts
  throwaway.
* **The simulator's IDS daemon crash-loops, and it is not our app.** On the seed
  installed on this Mac (see the prerequisites table),
  `identityservicesd` dies with `EXC_BREAKPOINT` roughly every 9 s and launchd
  respawns it (`launchctl list` shows exit status `-5`). The faulting frames are
  Apple's own: `XPCPeerRequirement.hasEntitlement(_:)` → `__XPC_API_MISUSE__` →
  `_assertionFailure`, reached from IDS/Key-Transparency/SystemConfiguration
  init inside a Swift `dispatch_once` — and `XPCPeerRequirement` is
  `@available(iOS, unavailable)` in the SDK, i.e. a macOS-only API reached on the
  simulator. Each crash raises a host **Problem Report** window that steals
  focus mid-run. Measured on a factory-erased device with nothing installed and
  nothing launched: **8 crashes / 70 s** headless and **4 / 45 s** with Device
  Hub attached — so it is not the app, not device state, and not the missing UI.
  `defaults write com.apple.CrashReporter DialogType none` does **not** suppress
  the window. The fix is to stop running a daemon po never uses:
  `test/surfaces/ios-sim.ts` now disables it in the simulator right after boot
  (`simctl spawn <udid> launchctl disable|bootout system/com.apple.identityservicesd`),
  which gives **0 crashes / 70 s**, survives a reboot, and leaves the app
  unaffected (smoke still passes). Note Simulator.app no longer exists —
  current Xcode ships it as `Contents/Applications/DeviceHub.app`
  (`com.apple.dt.Devices`).
* **A warm `po://` URL was silently dropped on iOS (fixed 2026-11-04).** The
  URL *does* arrive — `simctl openurl` needs no tap, and the pre-approved
  `com.apple.launchservices.schemeapproval.plist`
  (`test/surfaces/ios-sim.ts::approveUrlScheme`) is honoured — but two app-side
  defects used to swallow it, and both are now fixed:
  (1) the scene hooks were installed only from UIKit's
  `application:configurationForConnectingSceneSession:options:` callback, which
  UIKit calls only when it creates a **new** scene session. Every later launch
  restored the persisted session, never asked for a configuration, and therefore
  never swizzled `TaoSceneDelegate` — so `scene:openURLContexts:` was Tao's
  unhooked original. `DeepLinkRuntime.installHooks` now swizzles the scene
  delegate class directly, at launch, independent of the session lifecycle.
  (2) `po_notify_app_activity_changed` / `po_notify_app_background` are resolved
  by Swift through `dlsym`, and nothing in Rust calls them, so the linker dropped
  them under `lto = "fat"` — `dlsym` returned null and every native lifecycle
  notification was a no-op. `src/platform/native_bridge.rs` now pins both
  exports with a `#[used]` static.
  Delivery is observable: `event="deep_link"` in `src/platform/deep_link/mod.rs`
  fires only when a launch URL is actually consumed. Before the fix it never
  appeared on `ios-sim` even though the URL sat in the plugin's pending slot.
* **A warm `po://` URL needed the app delegate's legacy hook too (fixed
  2026-11-04).** The scene hooks cover a *cold* launch, but a URL opened while
  the app is **already running** arrives through the app delegate's
  `application:openURL:options:` — the scene callbacks are not called for it,
  and UIKit swallowed the URL (the plugin's `pendingUrl` stayed empty).
  `DeepLinkRuntime.installHooks` now swizzles that method on the app delegate
  as well, so both the cold and the warm path deliver.
* **The URL must not race the launch.** `simctl openurl` cold-launches the app
  when nothing is running, and a pending `simctl launch --terminate-running-process`
  then kills that instance and relaunches it **without** the URL — the app never
  sees it, while `PO_E2E_PAIR_PAYLOAD` still carries the payload and makes the
  pairing succeed, hiding the loss. The native surfaces therefore wait for the
  app's first log line before delivering the link
  (`test/surfaces/ios-sim.ts::waitForAppStart`, and the same guard in
  `test/surfaces/macos.ts::openUrl`).
* **Android holds one pending deep link.** A second `po://` intent arriving
  before the first is consumed replaces it; the app logs
  `pending deep link was replaced before it was consumed` (without the URL — a
  pairing link embeds the PIN). If that warning shows up in a run, the link was
  dropped, not merely delayed.
* **Re-applying the same payload must not cancel a live pairing.** The deep
  link and the launch environment usually carry the *same* link, and the link
  arriving after the form auto-submitted used to abort the in-flight attempt
  (`event="pairing_rejected" reason="superseded"`). `apply_payload` in
  `src/components/auth/pairing/pair_mode.rs` now treats an unchanged payload as
  a no-op.
* **`PO_E2E_PAIR_PAYLOAD` is the joiner's fallback, not a substitute.** A joiner
  can complete on the **env seams alone** (`PO_E2E_PAIR_PAYLOAD` + PIN +
  password route a signed-out app to `/pair`), which is why `S1` passed on
  `ios-sim` while deep-link delivery was broken. The env seam proves the pairing
  path, not the URL path; only `event="deep_link"` proves the URL reached the app.
* **Two bundles claim `po://` on the host, so `open` is not a delivery channel.**
  The macOS app and the iOS **simulator** build share the bundle id
  `com.debanjanbasu.po`, and LaunchServices resolves `po://` by bundle id —
  whichever build it favours receives the URL, so `open 'po://…'` (and
  `open -a <bundle> po://…`) can land in the simulator app while the harness
  watches the macOS peer's stdout, and the URL silently vanishes. The macOS
  surface therefore uses the Apple Event path instead
  (`osascript -e 'tell application id "com.debanjanbasu.po" to open location
  "po://…"'`), which targets the app by id and is the same `kAEGetURL` event
  the app's handler (`DeepLinkURLEventHandler`) already receives. Keep the
  `event="deep_link"` marker in the run's expectations — it is the only proof
  that the URL reached the peer being watched.
* **Parallel runs starve a native peer.** Web scenarios parallelise cleanly — the
  static server binds an ephemeral port (`listen(0)`) and each peer gets its own
  browser context, so nine of them ran green in ~1.5 min each. A *native* peer does
  not: run alongside a batch of web runs, the macOS joiner of `S1 --surfaces
  web,macos` printed the Veilid banner and then nothing for the full two-minute
  budget, and the harness reported `SEAM MISSING … no event= line reached the log`.
  The same command passes in 47 s on an idle machine, with a full marker stream
  (`dht_ready`, `dict_synced`, `paired`, `queue_drained`, `watch_reconciled`). Read a
  silent native log as contention first, not as a broken seam — and keep native runs
  out of the parallel batch.
* **Wedged Gradle daemons.** An interrupted Android build can leave a Gradle/Kotlin
  daemon that makes the next one hang instead of failing; `sh ./gradlew --stop` (in
  `target/dx/po/debug/android/app`) plus `pkill -f KotlinCompileDaemon` clears it.
* **Device readiness.** Locked phones report `unavailable` and refuse install/launch
  (the existing script already polls unlock); `devicectl device info lockState` is the
  cheap pre-flight check.
* **Stale build artifacts.** Surfaces used to build only when their artifact was
  *missing*, so a bundle from a previous day kept running while the source moved on —
  a web peer served a build from an older `dx`/veilid revision, and a source fix never
  reached it, and one device run exercised a five-hour-old binary. Every surface now
  rebuilds when its artifact is older than the newest source file, through one shared
  guard: `test/lib/build.ts::ensureBuilt` (the native surfaces differ only in the build
  step they pass it).
* **Container access is not guaranteed** for xtool-signed installs, so file-level
  assertions (`devicectl device copy`, `info mountpoint`) may be unavailable; design
  assertions on logs/UI first and treat the data container as a bonus.
* **The device attaches one console listener at a time.** A second
  `devicectl … launch --console` while the first bridge is still attached
  produces an app whose stdout reaches **neither** bridge — the run then reports
  `SEAM MISSING` with an empty log. The device surface therefore stops the old
  bridge before the URL launch (`--terminate-existing` kills the app process,
  not the bridge), keeps `--console` on the URL launch (without it the app
  starts without `DEVICECTL_CHILD_*`, so `PO_E2E` would be off), reuses the
  peer's launch env (otherwise the restarted app loses `PO_E2E_PAIR_*` and
  sits on an empty form), and waits for the new bridge to produce output before
  the scenario reads the log.
* **A physical iPhone keeps its session between runs — so the device surface
  isolates it.** The app restores the account it was paired into, so a pairing
  link opened on it is *correctly* refused ("Pairing links only work on a
  device that isn't signed in") and `event="paired"` never appears. The device
  surface now gives each run its own `PO_INSTANCE` (derived from the run
  directory's timestamp), which points the Veilid data dir and the namespace at
  a fresh location — the phone gets the same clean slate the simulator gets
  from its uninstall, without paying for a reinstall. `CFFIXED_USER_HOME` does
  **not** work for this: it does not isolate the app's state (the DHT identity
  stays the same).

## Evidence bundle

`test/runs/<timestamp>-<SCENARIO>-<surfaces>/` per run (override with `--evidence-dir`):
one log per peer, the screenshots each step took, and `result.json` with the git revision,
the surface/peer roster, per-step outcomes and the marker timeline. Timeouts write the
peer's log tail and a screenshot next to the failure rather than hanging.

`result.json` carries `skipped` (and each step's own `skipped`) next to `ok`: the capability
steps this run did not assert, because `skip` was recorded rather than thrown. A PASS with
a non-empty `skipped` proved less than its `ok` suggests — `run.log` says the same on the
way out — and a goal of "every scenario green" has to be read together with it.

The timeline carries **observations, not only waits**: every `waitForSignal`,
`waitMarkerField` and `waitMarkerCount` records the line it matched, and so do the identity
read (`support.ts::recordIdentity`) and each negative assertion — S9's absent `paired`, S5's
absent `dead_lettered`, QR's absent `qr_decode_failed`, S8's pinned
`watch_reconciled record=app_state`, and S14's post-dismiss state. A PASS whose timeline
shows the waits but none of the assertions is the anomaly, not the norm: a negative that
was never recorded is indistinguishable from one that was never checked.

`run.log` opens with the build decision for each surface (`<surface> build missing —
building` / `stale — rebuilding`), and each surface resolves its build artifact and device
identifiers into `Surface.build`; the `spawn` line at the top of a web/macOS peer log names
the artifact that actually ran. That bundle is the artifact a release check reads, and
it maps 1:1 onto the correlation recipe in
[docs/debugging.md](debugging.md#two-device-debugging-recipe-pairing-sync).
