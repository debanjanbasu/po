# po

Local-first, decentralized, peer-to-peer, reactive AI platform built with Rust + Dioxus.

**Platforms:** macOS / Linux / Windows · Web (WASM) · iOS / Android  
**Rust:** `nightly` (edition 2024); **Default feature:** none

---

## Features

- Account auth with username/password + PIN + biometrics
- Device pairing via QR/PIN/password with staged post-admission commit and background hydration
- End-to-end encrypted SMPL-record chat with HPKE-wrapped conversation keys, durable invites and sender-proof signaling
- Transactional DHT sync with atomic multi-record writes and Automerge rescue on hydration
- TOTP authenticator setup (QR or manual)
- Local snapshots + retention + backup export/restore, including the native iOS document picker
- Activity bar/modal for queue, DLQ, training, recompression, and background errors
- Shared native QR scanning across pairing, contacts, and TOTP; Android uses Google Play Services Code Scanner (no app `CAMERA` permission), iOS uses VisionKit with an AVFoundation fallback

Authenticated navigation defaults to Chat. Chat, TOTP, and Account are implemented; AI, Calendar, and Calls remain Coming soon placeholders.

---

## Quick Start

### Prerequisites

- [Rust nightly](https://rustup.rs)
- [LLVM](https://llvm.org) for web builds (`zstd-sys` C -> WASM)
- Dioxus CLI

```bash
brew install llvm
cargo install \
  --git https://github.com/debanjanbasu/dioxus.git \
  --branch main dioxus-cli
```

Configure tools in the invoking shell (or CI) directly: for example,
`NO_DOWNLOADS=1` tells `dx` not to fetch helper tools such as esbuild, and the
Android chain uses `ANDROID_HOME`/`JAVA_HOME` from the shell.

Dioxus and Manganis use the maintained `main` branch of
`https://github.com/debanjanbasu/dioxus`.
Keep `dx` on the same verified Git revision from `Cargo.lock`; matching alpha
version strings alone is insufficient. Rust uses the rolling `nightly` channel
through `rust-toolchain.toml`; CI installs the current nightly. Run
`rustup update nightly` locally to refresh.

### Development

```bash
# Desktop
~/.cargo/bin/dx serve --platform macos --hot-patch false
~/.cargo/bin/dx serve --platform windows --target x86_64-pc-windows-msvc --hot-patch false
~/.cargo/bin/dx serve --platform linux --target x86_64-unknown-linux-gnu --hot-patch false
~/.cargo/bin/dx serve --platform linux --target aarch64-unknown-linux-gnu --hot-patch false

# Web
~/.cargo/bin/dx serve --platform web --hot-patch false

# Mobile build
~/.cargo/bin/dx build --platform ios
~/.cargo/bin/dx build --platform ios --target aarch64-apple-ios-sim
~/.cargo/bin/dx build --platform android

# iOS physical device (xtool signs+installs, devicectl launches; see script header)
# One-time setup: node scripts/dx-setup.ts installs xtool (`xtool-org/tap/xtool`)
# and offers `xtool auth login` (Apple ID; free team works). First run prompts to revoke
# existing Apple Development certs and mints xtool's own; keep xtool as the
# sole signer afterwards. Trust the developer cert on the device once
# (Settings > General > VPN & Device Management).
# Dev builds install as XTL-<team>.com.debanjanbasu.po (xtool namespacing):
# URL schemes work; universal links only match release-signed builds.
~/.cargo/bin/dx build --ios --target aarch64-apple-ios
./scripts/ios-device-deploy.sh
```

### Debugging

Live log capture per platform, the in-app Activity/Background-Errors panel,
cold-state resets, and Veilid error triage: [docs/debugging.md](docs/debugging.md).

Optional Veilid performance counters are available with `--features veilid-stats`.
Add `aimd-dht` only for controlled comparisons; adaptive DHT byte gating is
experimental and disabled in normal builds. The tandem harness adds
`--features e2e-seams` to its release builds so they still carry the
[`e2e` harness seams](docs/device-testing.md); the feature is off by default and
never set for a distributed build, whose accessors compile to no-ops.

### Release Baselines

```bash
# macOS (Apple Silicon)
~/.cargo/bin/dx build --release --platform macos
./scripts/fix-bundle-case.sh target/dx/po/release/macos # inject the Dock icon dx build skips

# iOS/iPadOS
~/.cargo/bin/dx build --release --platform ios
./scripts/fix-bundle-case.sh target/dx/po/release/ios # inject the launcher icons dx build skips

# Android (generic aarch64)
~/.cargo/bin/dx build --release --platform android
./scripts/android-launcher-icons.sh target/dx/po/release/android # dx ships default icons; swap in ours + regradle

# Linux/Windows: x86-64-v3 and ARM64 (generic Linux / Oryon Windows)
~/.cargo/bin/dx build --release --platform linux
```
CI release verification covers Linux and Windows x86_64/ARM64 plus macOS ARM64,
iOS and Android remain ARM64-only platform builds. The Android leg is the only
job that signs: it writes the keystore and its passwords out of the `release`
environment at run time into a gitignored path, appends `[bundle.android]`, and
deletes both again before its diff guard.

Web release builds have two knobs that bite. `[web.wasm_opt] extra_features` in
`Dioxus.toml` is an explicit feature list mirroring the wasm32 rustflags in
`.cargo/config.toml`; never `--all-features`, which stamps the shipped module with
proposals rustc never targeted (24 advertised against 11 now). A **green build
proves nothing** — a wasm-opt failure is swallowed and the unoptimized module is
copied over the output, so verify by size or by dx's own `Running wasm-opt:` log
line. And dx rebuilds that section's debug setting from its command line
(`--debug-symbols`, default true), so releases pass `--debug-symbols=false`; it
also drops wasm-bindgen's debug pass, which is most of the JS shrink.

Distribution CPU baselines live in `.cargo/config.toml`, including local release
builds. A `RUSTFLAGS` override replaces configured flags; do not accidentally
drop the shared Tokio/nightly flags when changing a CPU baseline. CI release
builds do not set `RUSTFLAGS` — they add the Linux lld link argument through
`cargo --config`, which joins the configured flags instead of replacing them.

**Do not add `-Zlocation-detail=none` to the release profile.** It makes every
`Location::caller()` return the same location, and Dioxus keys each
`GlobalSignal`'s registry entry by its caller location. All global signals in
the app then collapse into one slot, resolving a signal of a different type
evicts the previous one, and reactivity breaks — the app hangs forever on the
startup splash because the session phase never reaches the router. The release
profile therefore does not set it; `trim-paths = "all"` already keeps filesystem
paths out of distributed binaries.

### Build Verification

```bash
# Desktop
cargo check --features desktop
cargo clippy --features desktop -- -D warnings
~/.cargo/bin/dx build --platform macos

# Web
cargo check --target wasm32-unknown-unknown --features web
cargo clippy --target wasm32-unknown-unknown --features web -- -D warnings
~/.cargo/bin/dx build --platform web

# Host toolchain for every platform (macOS/Linux/Windows; idempotent, re-run after
# SDK/Xcode/dx updates). Prints the exports; add --yes to persist them.
eval "$(node scripts/dx-setup.ts)"
~/.cargo/bin/dx build --platform android

# Every JS/TS file (harness, scripts, landing) — strict and type-aware
npm ci
npm run check       # oxlint + TypeScript

# Serialized tandem/e2e run (arguments pass through to test/run.ts)
npm run e2e:locked -- --scenario S1 --surfaces web
```

The two `cargo clippy` lines above compile only the desktop and web branches, so
`#[cfg(target_os = "android")]` code is never linted by them. After changing an
Android-only path, lint that branch too — it needs the NDK exports from
`scripts/dx-setup.ts` for the `cc` crates, and the NDK ships only API-suffixed
clang wrappers, so name one explicitly (pick the newest in
`$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/*/bin` that matches the `[android]`
`min_sdk`):

```bash
B=$(ls -d "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin | head -1)
CLANG=$(ls "$B"/aarch64-linux-android*-clang | sort -V | tail -1)
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG" \
CC_aarch64_linux_android="$CLANG" \
AR_aarch64_linux_android="$B/llvm-ar" \
cargo clippy --target aarch64-linux-android --features mobile -- -D warnings
```

`scripts/dx-setup.ts` owns the host toolchain `dx` needs — web, desktop, Android, iOS —
and runs its independent sections concurrently: the Android chain (JDK, SDK packages,
NDK), the Rust components/targets this repo builds with, `NO_DOWNLOADS` tool
discovery, and the platform extras. Its exports resolve at shell start; re-run it
after a toolchain or `dx` update so the next shell sees the refreshed versions.

Android's min/target/compile levels are the `[android]` block in `Dioxus.toml`. The script
installs the newest OpenJDK within the range Gradle accepts, the newest
`platform-tools`/`platforms;android-*`/`build-tools;*` plus the API-matched set the build
pins, and the newest NDK, then asserts the `aarch64-linux-android<API>-clang` wrapper the
pinned Dioxus CLI demands from `[android] min_sdk` — only recent NDKs ship it. `dx` has no
default SDK path, so a shell without those exports
reports *"Android not installed properly"*; on a terminal the script says whether your
shells already resolve them and asks before changing anything. With `--yes` it adds a
generated block that exports the variables directly in your shell startup files (`setx` +
user `PATH` on Windows, WebView2 via winget); there is no env-file mechanism. Native
modules inherit the app's compile SDK; device-side prerequisites and
the tandem harness are in [docs/device-testing.md](docs/device-testing.md#prerequisites).
The macOS run also installs `xtool` and offers its Apple ID login. Android requests
ACCESS_LOCAL_NETWORK at runtime for LAN peers. Denial leaves local-first startup
available while Android blocks LAN traffic; iOS declares Tao's scene manifest
and installs scene URL/lifecycle hooks before launch; macOS retains AppKit lifecycle handling.
SDK compilation is not device certification: cold/warm links, permissions,
camera cancellation, and background/foreground recovery still require target-device
checks in [docs/debugging.md](docs/debugging.md).

CI workflows warm the Cargo graph with per-job/profile Rust caches so builds
reuse prior dependency compilation; keys separate arch/profile/job families and
invalidate on the committed lockfile.

`.github/workflows/quality.yml` checks formatting, desktop/Web Clippy, Swift, and
the landing build on PRs. `release.yml` replaces placeholder PGO with bounded
optimized builds and non-executing binary/architecture checks; it uploads
**verification artifacts only**, without signing material or automatic
publication. Key derivation takes no build-time secret, so every build of a
revision produces the same account keys. Linux/macOS raw-binary archives are not asset-complete
application bundles. Windows packaging uses the locked `dx` revision and the
bundle publisher in `Dioxus.toml`. The macOS CI runner image is a preview one,
not the locally verified GA Xcode build; the label each job uses is in
`.github/workflows/`.

Use `cargo fmt --all -- --check` for a non-mutating formatting check. The locked
`dx fmt --check` implementation can rewrite source before reporting its result.

`dx build`/`dx serve` regenerate `assets/tailwind.css` automatically. Under
`NO_DOWNLOADS=1`, `dx` resolves Tailwind, `wasm-bindgen`, `wasm-opt`, and
`esbuild` with `which()` instead of downloading them. `scripts/dx-setup.ts`
prepares those paths and exports `NO_DOWNLOADS=1`: run `npm ci` for the npm
Tailwind CLI, install the locked `dx` once with downloads enabled so it creates
its managed tools directory, then re-run
`eval "$(node scripts/dx-setup.ts)"` after every `dx` update to refresh the
versioned tool dirs it puts on `PATH` (`node_modules/.bin`, managed
`wasm-bindgen`/`binaryen`/`esbuild`). `TAILWIND_BIN` remains an optional
override naming a standalone Tailwind executable; Windows CI uses it for the
x64 standalone binary. `.nvmrc` uses nvm's `node` alias, while `engines.node`
only sets the minimum required by native TypeScript scripts; `.npmrc` enforces
that floor. Run `nvm install` to refresh the local Node alias.

Manual one-off Tailwind regeneration, if ever needed:

```bash
npx @tailwindcss/cli -i tailwind.css -o assets/tailwind.css
```

### Brand Assets

The logo is pixel art — the nerd mascot from the 404 page, whose round
glasses form the mark (two lenses, two devices, one identity), in the palette of
`assets/orig-content/vector-404-not-found.svg`. Everything is generated from
the pixel map in `scripts/gen-icons.ts` and checked in; edit the generator,
never the outputs:

```bash
node scripts/gen-icons.ts   # requires Node's built-in TypeScript type stripping
                           # and ImageMagick (`brew install imagemagick`)
```

| Output | Used by |
|---|---|
| `assets/logo.svg` | in-app logo, web/app SVG favicon, landing hero |
| `assets/favicon.ico` | web fallback favicon (16/32/48) |
| `assets/icons/ios/AppIcon*.png` | iOS launcher icons — `CFBundleIcons` in `native/apple/Info.ios.plist`; copied into the `.app` by `scripts/fix-bundle-case.sh` |
| `assets/icons/icon.icns` | macOS Dock icon — `CFBundleIconFile` in `native/apple/Info.macos.plist`; copied into the bundle by `scripts/fix-bundle-case.sh` |
| `assets/icons/android/*.xml` | Android adaptive-icon drawables — injected by `scripts/android-launcher-icons.sh` |
| `landing/public/{logo.svg,favicon.ico,apple-touch-icon.png}` | landing site |

`[application] name = "po"` in `Dioxus.toml` makes `dx build` emit `po.app`
directly; `scripts/fix-bundle-case.sh` only injects the declared Apple icons,
and `scripts/ios-device-deploy.sh` invokes it before signing and installing. The copy
steps exist because the locked `dx` revision (`Cargo.lock`) has no icon pipeline
for iOS/macOS/Android (`[bundle].icon` only affects `dx bundle`).

---

## Architecture

### Technology Stack

| Layer | Tooling | Notes |
|---|---|---|
| UI | Dioxus (git) + Router | Cross-platform reactive UI |
| Styling | Tailwind v4 + DaisyUI + iconify plugin | `bumblebee` + `bumblebee-dark`, an orange/amber palette |
| Network + DHT | Veilid | P2P, DHT, app messaging; optional AIMD and stats features |
| Serialization | recapn (Cap'n Proto) + automerge | Dual-format local storage |
| Conflict resolution | automerge + autosurgeon | CRDT merge via `doc.merge()` |
| Crypto | Veilid AEAD + Argon2 | E2E encrypted persisted data |
| Compression | zstd level 12 + dictionary | Structured + plain domains |

### Data Directories

Native `create_config()` passes the preserved `.veilid` root to
`VeilidConfig::new("po", "debanjanbasu", "com", storage_root, None)`, which
places `protected_store/`, `table_store/`, and `block_store/` below it. Web
passes no filesystem root, keeping Veilid's empty directory settings and
`APP_ID`-namespaced IndexedDB/protected-store data. Android prefers the
Activity's `getFilesDir()`; when unavailable it searches `HOME` then the
current directory for the app's `files/` directory, finally using
`./com.debanjanbasu.po/files/.veilid/`.

| Platform | Veilid storage root |
|---|---|
| macOS | `$XDG_DATA_HOME/Library/Application Support/com.debanjanbasu.po/.veilid/` or `$HOME/Library/Application Support/com.debanjanbasu.po/.veilid/` |
| iOS | `$CFFIXED_USER_HOME/Library/Application Support/com.debanjanbasu.po/.veilid/` (falls back to the current directory, then `/private/var/mobile`) |
| Windows | `%APPDATA%\com.debanjanbasu.po\.veilid\` or `%LOCALAPPDATA%\com.debanjanbasu.po\.veilid\` |
| Linux/Unix | `$XDG_DATA_HOME/com.debanjanbasu.po/.veilid/` or `$HOME/.local/share/com.debanjanbasu.po/.veilid/` |
| Native desktop without a usable data/home path | `./.veilid/` |
| Android | Activity `getFilesDir()/.veilid/`; fallback as described above |
| Web | No filesystem root; IndexedDB and protected-store keys use the `APP_ID` namespace |

On Web, TableStore database names use the `APP_ID` prefix, and protected-store keys use
the `__veilid_protected_store_<APP_ID>_` prefix.

See `src/veilid/config.rs`.

### Data and Sync Flow

Local-first mutation → durable pending operation → per-lane ordered drain → DHT reconcile/write → success cleanup or retry/DLQ. Deleted-account tombstones block stale replay. See [docs/sync-pipeline.md](docs/sync-pipeline.md) for dispatch and queue invariants; [docs/dht-architecture.md](docs/dht-architecture.md) for record layouts and local storage.

---

## User Flows

### Route Gates

1. `AuthRoute` (unauthenticated): login / signup / pairing
2. `PostRegister` (needs setup): terms / DOB / PIN / biometrics (native targets auto-prompt once after a valid PIN unlocks the biometrics step)
3. `Route` (authenticated): Chat · TOTP · AI · Calendar · Calls · Account

Authenticated `/` and `/login` redirect to `/chat`; login/signup remains available at unauthenticated `/`.

Account preferences hydrate strictly from local storage in the stable app root
before either authenticated router mounts. Hydration and route-persistence gates
are admission/epoch-scoped; read errors remain visible and retry rather than
overwriting stored preferences with defaults. Pending deep links are reactive,
take precedence over saved navigation, and never leave persistence disabled.
Committed `AppState` revisions refresh nav/theme without restoring the route
again or reopening a completed gate.

### Signup

```text
random account ID -> Argon2(password, salt = account ID) -> account-bound DHT secret (constant domain key) -> encrypted keypair -> local save
-> enqueue identity create -> onboarding
-> native biometric prompt (if available after valid PIN) -> main app
```

### Login

```text
credential check -> decrypt keypair -> stage remember-me
-> biometric (if enabled) -> PIN fallback gate
-> complete admission -> apply staged effects -> hydrate/sync
```

### Pairing and Contact Input

- Pairing accepts `po://pair?data=...`, `https://po-app.pages.dev/p/{payload}`, or raw payload; v1 pairing PINs are exactly 12 digits and expire after 5 minutes.
- Contact add accepts: `po:username#dht_key`, `https://po-app.pages.dev/c/{username}/{dht_key}`, encoded deep link, or `username|dht_key`.
- Pairing and share actions trigger reactively once valid inputs are provided; superseded async attempts are discarded.

---

## DHT Record Structure

The authoritative identity and conversation layouts, reserved subkeys, encryption/compression modes, ownership, and archive behavior are in [docs/dht-architecture.md](docs/dht-architecture.md). Keep this README as a navigation index, not a second schema specification.

---

## Security Model

| Mechanism | Detail |
|---|---|
| Password hashing | Argon2, salted with the 16-byte account ID |
| DHT AEAD key | Password hash + random account ID under the initial constant domain separator |
| Storage encryption | AEAD on DHT/local persisted payloads (local rows encrypted by Veilid's TableDB under a device key in the OS protected store; insecure keyring in debug and e2e builds, browser storage on web) |
| Identity key material | Account-bound encrypted keypair blob on identity subkey 3, AEAD-bound to `po:keypair:v1` |
| Password rotation | Re-encrypt identity-sensitive subkeys |
| PIN | Argon2 hash (local + synced security state) |
| Biometrics | Platform-native auth bridges |
| Pairing | v1 QR payload + exact 12-digit, five-minute PIN + account password; remember-me wrap key remains device-local |
| Backups | Initial v1 AEAD-header-bound format using the account-bound DHT key |
| Deep links | `po://` + universal links under `https://po-app.pages.dev` |

---

## Compression
Structured, Plain DHT transport frames, and local TableDB rows use zstd level 12 (`ZSTD_LEVEL = 12`; level 22 was dropped because web runs the codec inline on the UI thread). See [docs/compression.md](docs/compression.md) for framing, dictionary lifecycle, and local-storage policy.

---

## Global State Catalog

This is the single catalog of process state and ownership. `GlobalSignal`s must be accessed from Dioxus runtime tasks, not `tokio::spawn`. Session-scoped writes pair `UserCtx` with a `SessionGuard` and hold the appropriate account commit lock through local commit; `sync::mutate` locks persisted-account ownership before its account lock. Domain-local statics and caches remain with their owning modules.

| State | Owner | Notes |
|---|---|---|
| `nav_bar_items()`, `last_active_route()`, `theme()`, `network_status()`, `conversation_meta()` | `src/global_state.rs` | Accessors for core app signals |
| `SESSION` | `src/session/phase.rs` | Auth state machine and current user source of truth |
| `ROUTE_RESTORED` | `src/router/helpers.rs` | Admission-scoped local hydration and route persistence gate |
| `PENDING_ACTION` | `src/platform/deep_link/mod.rs` | Typed pending action; native bridges retain URLs before runtime startup |
| `CrdtModel::revision_signal()` | `src/storage/store_trait/mod.rs` | Per-model `GlobalSignal<u64>` revision notifications after successful local commits |
| `SNAPSHOT_REVISION` | `src/storage/snapshots/mod.rs` | Invalidates snapshot-history resources |
| `DEEP_LINK_TOAST` | `src/components/overlays/deep_link_toast.rs` | Deep-link toast state |
| `SHOW_ACTIVITY_MODAL` | `src/components/overlays/mod.rs` | Overlay-local UI signal |
| `SYNC_STATUS`, `PENDING_OPS_REVISION`, `TRAINING_STATUS`, `UPGRADE_STATUS`, `HYDRATION_STATUS`, `PAIRING_SYNC_STATUS`, `BACKGROUND_ERRORS` | `src/storage/mod.rs`, `src/storage/background_errors.rs` | Operational status signals plus the monotonic queue-content revision |
| `NEXT_UPGRADE_STATUS_RUN` | `src/storage/mod.rs` | Allocates owner IDs for progress cleanup |
| `CONV_MANAGER`, `WATCH_LOCKS` | `src/chat/manager.rs` | Process-local watch lifecycle manager and per-conversation watch locks; not `GlobalSignal`s |
| `TYPING_GENERATION`, `TYPING_VERSIONS` | `src/chat/models/conversation.rs` | Monotonic typing-indicator generations and per-conversation typing state/version counters |
| `CACHES` | `src/chat/storage/crud.rs` | Account-scoped in-memory `LocalConversation` cache |
| `PENDING_INVITES`, `DEFERRED_PENDING_INVITES` | `src/chat/invite/types.rs` | Pending conversation invites signal and pre-runtime deferred invite queue |
| `SENDER_IDENTITY_CACHE` | `src/chat/signal/dispatch.rs` | Cached public-key-to-identity-record derivations for incoming signals |
| `src/user/identity_watch.rs::LIFECYCLE` | `src/user/identity_watch.rs` | Serializes session-pinned identity record watch start/stop/reattach with teardown |
| `src/user/child_watch.rs::LIFECYCLE`, `src/user/child_watch.rs::WATCHED` | `src/user/child_watch.rs` | Serializes child-watch reconciliation with teardown and tracks pinned child record keys |
| `INITIAL_RESYNC_OK`, `HYDRATION_BURST` | `src/user/watcher/burst.rs` | Tracks initial identity resync completion and short-lived post-login hydration burst window |
| `ACTIVE_SHARE`, `REMOTE_BEACON` | `src/user/pairing_beacon.rs` | Active QR pairing share state and remote pairing-progress beacon signals |
| `IDENTITY_SYNC_LOCKS` | `src/user/password_rotation.rs` | Per-account locks serializing password rotation and identity re-encryption |
| `LOCAL_ROUTE`, `ROUTE_CACHE` | `src/chat/signal/*` | Route publish/cache state |
| `SYNC_QUEUE`, `IS_PROCESSING`, `DELETED_ACCOUNT_TOMBSTONES` | `src/sync/queue/` | Queue runtime and stale-dispatch gate |
| Pending-op indexes + sequence | `src/sync/pending_ops/mod.rs` | Durable deduplication and monotonic sequence |
| Veilid API/update/dispatcher statics | `src/veilid/core/*`, `src/veilid/change_dispatcher.rs` | API lifecycle and dirty-key dispatch |
| `STORE_LOCKS` / `RECORD_LOCKS` | `src/helpers/store_lock.rs`, `src/veilid/store/lifecycle.rs` | Per-key local RMW and Veilid record lifecycle locks |
| Compression registry/pools/coordination | `src/compression/*` | Dictionary registry, readiness/update coordination, codec pools, recompression |
| `COLLECTOR_GENERATION` | `src/compression/samples.rs` | Fences sample-flush restoration after reset/account switch |
| Session epoch + crypto cache | `src/session/epoch.rs`, `src/user/session_crypto.rs` | Session guard API and per-user crypto cache |
| `DEVICE_WRAP_KEY` | `src/user/store/mod.rs` | Process-local remember-me wrap-key cache |
| `DEVICE_ID`, `DEVICE_LABEL` | `src/platform/device_id.rs` | Cached persistent device identifier and user-facing device label |

Add state in its narrowest owner, use nearby access/mutation helpers, and update this catalog when ownership or lifecycle changes. See [docs/concurrency.md](docs/concurrency.md) for concurrency rules.

## Project Structure

The annotated source tree below is the project structure map. Use it rather than maintaining a second directory summary in `AGENTS.md`.

```text
src/
  app/                 # shell/startup/router orchestration
  router/              # route enums + nav layout + dock reorder
  components/          # auth, Chat/TOTP, Coming soon AI/Calendar/Calls, settings, shared forms/overlays UI
  chat/                # models, record sync/hydration/archives, watcher, signaling, storage, invites
  user/                # auth, onboarding, pairing/biometric, identity record, sync, store, crypto
  session/             # session epoch + phase state machine
  sync/                # queue runtime, pending-op persistence, exhaustive OpType dispatch
  storage/             # SyncSurface/CrdtModel, dual-format storage, snapshots, status, schema guard
  compression/         # dictionary IDs/codec/trainer/recompression/training queue
  veilid/              # API lifecycle, DHT/table store, transactions, update ingress, dispatcher
  platform/            # deep links, sharing/backup picker, QR, lifecycle, device ID, window chrome
  helpers/             # crypto, async utilities, unbounded fan-out, binary framing, Cap'n Proto, locks
  models/              # app state, pending operations, TOTP models
  schemas/             # Cap'n Proto schema definitions
  bin/                 # dictionary-training sample generator
shared/                # Tailwind theme tokens shared by app and landing site
native/                # Apple and Android platform bridges/manifests
landing/               # Astro site for universal links and app download/redirect UX
scripts/               # build, deployment, icon generation, and CI helpers
```

### Coding Policy

See [AGENTS.md](AGENTS.md) for coding policies and workflow standards, including boundary and form-input rules.

---

## Key Derivation

Auth key derivation uses **no build-time secret**. The credential hash is Argon2id over the
password, salted with the account's 16-byte ID — public by design, which is what makes
pairing reproducible; so equal passwords never produce equal stored hashes either. The DHT
`SharedSecret` is then `derive_shared_secret(material, DHT_SECRET_DOMAIN_KEY)`, where the
key is a compile-time constant domain separator rather than a secret, and the initial material
is `"po:dht-secret:v1\0" ‖ account ID ‖ credential hash`. Entropy comes
from the Argon2 hash alone: only someone who knows the password can reproduce it.

```bash
cargo build --release --features desktop
```

Because the derivation has no per-build input, every build of a revision derives the same
account keys, no CI workflow has to plumb a secret, and there is nothing to leak from a
release artifact. Pairing payload keys use
`derive_shared_secret("po:pair:v1:" ‖ credential hash ‖ PIN, PAIRING_DOMAIN_KEY)`, under
their own constant domain key.

---

## Deep-Dive Documentation

- [docs/dht-architecture.md](docs/dht-architecture.md)
- [docs/sync-pipeline.md](docs/sync-pipeline.md)
- [docs/watch-system.md](docs/watch-system.md)
- [docs/peer-signaling.md](docs/peer-signaling.md)
- [docs/concurrency.md](docs/concurrency.md)
- [docs/compression.md](docs/compression.md)
- [docs/ui-conventions.md](docs/ui-conventions.md)
- [docs/debugging.md](docs/debugging.md)
- [docs/device-testing.md](docs/device-testing.md)

## Contributing

See [AGENTS.md](AGENTS.md) for coding policy, architecture constraints, and workflow standards.
