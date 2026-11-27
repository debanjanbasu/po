# `e2e.yml` — status and what to fix before it gates PRs

`e2e.yml` runs the tandem harness ([docs/device-testing.md](../../docs/device-testing.md),
[test/README.md](../../test/README.md)) on GitHub-hosted runners. The harness itself is
verified locally; the runner configurations are being brought up by manual dispatch
(`workflow_dispatch` only, on purpose: macOS minutes bill at $0.062/min on this private
repo — Linux x64 $0.006, Linux arm64 $0.005).

**Exact PR-gate criterion:** `web` becomes eligible only after **two consecutive green
`main` runs**, meaning two consecutive completed `workflow_dispatch` runs of `e2e.yml` on
the `main` branch in which the `Web tandem (ARM64)` job succeeds. A failed or cancelled
`web` job in a `main` run resets the count to zero; local runs and runs on other branches
do not count. Only after those two runs should the workflow add a `pull_request` trigger
and the branch protection rule require `Web tandem (ARM64)`. The `web` job has no
`continue-on-error`, so its failure will fail that future PR gate. There is deliberately
no `pull_request` trigger yet; `apple` and `android` remain non-gating.

## Fixed on the runner

Each of these cost a full dispatch to find, and each failed all three jobs at once:

| Symptom | Cause | Fix |
|---|---|---|
| `dx: command not found` (exit 127) after a successful 12m49s `cargo install` | `$GITHUB_PATH` only affects **later** steps, and `dx` was invoked bare in the same step that installed it | `scripts/ci-setup-dx.sh` resolves it by path, asserts it reports the locked revision, and is the one copy all three jobs call |
| `Path …/assets/tailwind.css not found` during `dx build` | `src/main.rs` embeds that file via `asset!`, and it is generated (gitignored), so a fresh checkout has none. `quality.yml` had a `stylesheet` job; `e2e.yml` had none | `.github/workflows/stylesheet.yml` is now a reusable workflow both call, and each dependent job keeps its `needs:` + download step |
| apple job: `wasm32-unknown-unknown` build failure | it builds the **web anchor**, but only the two Apple triples were installed | added `--target wasm32-unknown-unknown` |

**Why the first two are single-sourced now.** Each of those mistakes existed in three
places (dx) or two (stylesheet), so each had to be found and fixed once per copy — the
first symptom above was fixed three times in one sitting. A correction applied to one
copy and not the others is invisible until that job fails in turn.

## Verified locally (2026-11-01)

| Run | Result |
|---|---|
| `node test/run.ts --scenario S1 --surfaces web,macos,ios-sim` | PASS |
| `node test/run.ts --scenario S1 --surfaces web,ios-sim` | PASS |
| `node test/run.ts --scenario S1 --surfaces web` | PASS |
| `node test/run.ts --scenario smoke --surfaces web,macos,ios-sim` | PASS |

## Verified on the runners (2026-11-07)

`Tandem e2e` run [42418495166](https://github.com/debanjanbasu/po/actions/runs/42418495166)
finished **success**, the first time any of these configurations ran:

| Job | Result |
|---|---|
| `stylesheet` | success |
| `web` | success — **all 14 scenarios pass** (799 s of scenario time) |
| `apple` | success — `S1` (199 s) on `web` + `macos` + `ios-sim`. Only the first two surfaces **pair**: `S1` drives `surfaces[0]` as the anchor and `surfaces[1]` as its one joiner, so the simulator was launched and then idled (see test/README.md § one joiner per run). The pairing that this run actually proves is web ↔ macOS |
| `android` | failure — it was still the x86_64 job; see the arm64 rewrite below |

The four defects that had to be fixed to get there, in the order they surfaced, are in
[Fixed on the runner](#fixed-on-the-runner) — each one failed **all three** jobs, so each
cost a dispatch. (A fifth, a missing build-time pepper that signup derived its keys from,
has since been removed entirely: key derivation no longer needs any build-time secret.)

**Android is the one job still unverified in CI.** It now targets an ARM64 runner and image
(see below); the local surface is exercised on this machine's own arm64 AVD.

### Android on ARM64

The job ran on x86_64 only because `/dev/kvm` was missing from the arm64 Ubuntu images
(actions/runner-images#14549, closed 2026-08-12). With that gone, arm64 is both possible and
simpler: an arm64 AVD installs the APK the repo actually builds by default
(`aarch64-linux-android`; `dx` pins `abiFilters` to the build triple), so the `--target
x86_64-linux-android` override disappeared with it. `google_apis_ps16k;arm64-v8a` is the
non-Play-Store arm64 image for API 37.2 — Play **services** are what the QR scanner needs
(`GmsBarcodeScanning`'s implementation lives in GMS), while the Play **Store** app is only
needed for install-time module download. An earlier LTS arm64 image is the fallback if
`/dev/kvm` ever disappears again; the job checks for it up front rather than degrading to
full QEMU emulation.

The Android job uses one emulator for two serial, disjoint partitions from the
registry-derived catalog: `smoke` runs alone as the fail-fast partition, then every other
scenario runs once in the remaining partition. A smoke failure skips the remainder; the
remaining partition collects its scenario failures before failing the job. There are no
parallel emulators or duplicated scenario runs.

## Verified locally (2026-11-08) — the web catalog

The `web` job's catalog is the registry's, one process per scenario: its step asks the
harness which scenarios a web run carries (`node test/run.ts --list --names --surfaces
web`) instead of restating them, so `test/scenarios/registry.ts` is the one file that
declares the catalog, and a scenario added there is swept with no workflow edit. A
native-only scenario drops out of that list; `--list` names what each remaining one still
cannot cover.

On 2026-11-08 the whole catalog registered at that date passed here, and the scenarios
registered since (`S14`–`S17`) have each passed locally too — `test/runs/` holds the
evidence for both.

That is the only surface with no hardware dependency, which is why it carries the whole
catalog: the scenarios that need a second real device, a camera, or an OS prompt still
record a SKIP rather than a result.

The anchor is always the **web** surface: it is the only one with a DOM to type the
account password into. `macos`, `ios-sim` and `ios-device` are joiners, which take the
pairing payload, PIN and password from their launch environment (`PO_E2E_PAIR_*`).

## Per job

| Job | Runner | State | What to fix before trusting it |
|---|---|---|---|
| `web` | ARM64 Ubuntu (see the workflow) | green on the runner (run [42418495166](https://github.com/debanjanbasu/po/actions/runs/42418495166), 2026-11-07) and locally, 2026-11-08 | Nothing in the job itself: apt clang/lld building the wasm target and Chromium running on the ARM64 image are both confirmed by that run. Before it gates PRs, it must meet the exact criterion above: two consecutive `workflow_dispatch` runs on `main` with this job green |
| `apple` | ARM64 macOS/Xcode image (see the workflow) | green on the runner for `S1` on `web` + `macos` + `ios-sim` (same run) | The simulator is launched and then idled — `S1` pairs `web` ↔ `macos` only, one joiner per run (test/README.md § one joiner per run) — and the `S18` LAN-locality step came after that run, so a windowless runner has not exercised it. A headless `simctl` boot and the `iPhone 18 Pro` device name are confirmed by the green `S1` |
| `android` | ARM64 Ubuntu (see the workflow) | not dispatched since the ARM64 rewrite; the surface runs locally (`S1` and `QR` on `web,android` PASS, in `test/runs/`) | The rewrite as a whole, and `adb`/`input text` against a *headless* AVD. The job needs `/dev/kvm` and checks for it up front — without it the arm64 AVD would run under full QEMU emulation |

## Known-bad commands that were fixed

* `node test/run.ts --scenario S1 --surfaces macos,ios-sim` — neither surface can sign
  in as the anchor. The web surface is the anchor.
* `dx build --platform ios --device` — selects the device triple and then demands an
  Xcode-managed signing identity this repo deliberately does not use (xtool is the
  signer). Use `--target aarch64-apple-ios-sim` for the simulator, and the
  `ios-device` surface for real hardware.

## A trap worth knowing

`otool -l` on a **simulator** binary reports `platform 7`, which reads like a device
build at a glance. It is not: `PLATFORM_IOSSIMULATOR` is 7, `PLATFORM_IOS` is 2. Use
`vtool -show-build` (it prints `platform IOSSIMULATOR`) when checking which one you have.

## The physical iPhone is not a CI surface

`ios-device` needs a signed build and a real phone, and the install step needs USB —
xtool reaches devices through usbmuxd, and macOS 15+'s usbmuxd has no lockdown
pair-record store, so a Wi-Fi-only install is not possible. No hosted runner has
either. The `apple` job covers the simulator instead; the phone stays a local
release gate (docs/device-testing.md § "iPhone: USB for the install, Wi-Fi for the run").
