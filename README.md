# oo

Local-first, decentralized, peer-to-peer, reactive AI platform built with Rust + gpui-ce.

> **Migration in progress:** the UI is moving from Dioxus to [gpui-ce](docs/migration-gpui-ce.md). New gpui UI lives in `src/ui`; the Dioxus tree (`src/components`) is frozen until the cutover.

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
- [Trunk](https://trunkrs.dev) for web builds (`cargo install trunk`)

```bash
brew install llvm
cp .env.example .env
```

`.env` is a local-only file and stays untracked — CI fails the build if it is
ever committed.

gpui and Veilid track their maintained `main` branches by git; `Cargo.lock` records the moving resolution — never trust version strings in prose. Rust uses the rolling `nightly` channel
through `rust-toolchain.toml`; CI installs the current nightly. Run
`rustup update nightly` locally to refresh.

### Development

```bash
# Desktop
cargo run --features desktop

# Web
trunk serve --features web

# iOS simulator (binary; test/surfaces/ios-sim.ts assembles the .app bundle)
cargo build --release --target aarch64-apple-ios-sim --features mobile

# Android: the gpui_android backend ships in the fork (see
# docs/migration-gpui-ce.md §1)

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
cargo build --release --features desktop

# iOS/iPadOS — gpui_ios is vendored in the fork (docs/migration-gpui-ce.md §1)
cargo build --release --target aarch64-apple-ios --features mobile

# Android — gpui_android backend; the gradle host modules in native/android
# wrap the Rust binary

# Linux/Windows: x86-64-v3 and ARM64 (generic Linux / Oryon Windows)
cargo build --release --features desktop --target <triple>
```
CI release verification covers Linux and Windows x86_64/ARM64 plus macOS ARM64;
iOS and Android remain ARM64-only platform builds. The Android leg builds the
Rust binary and assembles the APK through the native/android gradle host.

Web release builds go through Trunk (`trunk build --release --features web`).
The wasm feature list in `.cargo/config.toml` must stay in lockstep with the
`wasm-opt` settings in `Trunk.toml` — never `--all-features`, which stamps the
shipped module with proposals rustc never targeted. A **green build proves
nothing**: verify the emitted module's size and `target_features` section
before shipping (the same discipline as the old dx flow).

Distribution CPU baselines live in `.cargo/config.toml`, including local release
builds. A `RUSTFLAGS` override replaces configured flags; do not accidentally
drop the shared Tokio/nightly flags when changing a CPU baseline. CI release
builds do not set `RUSTFLAGS` — they add the Linux lld link argument through
`cargo --config`, which joins the configured flags instead of replacing them.

**Do not add `-Zlocation-detail=none` to the release profile.** Dioxus keys each
`GlobalSignal`'s registry entry by its caller location (the frozen UI still
relies on this until the cutover); collapsing them breaks reactivity — the app
hangs forever on the startup splash. `trim-paths = "all"` already keeps
filesystem paths out of distributed binaries.

### Build Verification

```bash
# Desktop
cargo check --features desktop
cargo clippy --features desktop -- -D warnings
cargo build --release --features desktop

# Web
cargo check --target wasm32-unknown-unknown --features web
cargo clippy --target wasm32-unknown-unknown --features web -- -D warnings
trunk build --release --features web

# Host toolchain for every platform (macOS/Linux/Windows; idempotent, re-run after
# SDK/Xcode updates). Prints the exports; add --yes to persist them.
# xtool (iOS device deploys): brew install xtool-org/tap/xtool && xtool auth login
# Android: JDK 17+, the Android SDK/NDK and cargo-ndk (docs/migration-gpui-ce.md §1)

# Every JS/TS file (harness, scripts, landing) — strict and type-aware
npm ci
npm run lint
```

The two `cargo clippy` lines above compile only the desktop and web branches, so
`#[cfg(target_os = "android")]` code is never linted by them. After changing an
Android-only path, lint that branch too — the NDK ships only API-suffixed clang
wrappers for the `cc` crates, so name one explicitly (pick the newest in
`$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/*/bin`):

```bash
B=$(ls -d "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin | head -1)
CLANG=$(ls "$B"/aarch64-linux-android*-clang | sort -V | tail -1)
CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG" \
CC_aarch64_linux_android="$CLANG" \
AR_aarch64_linux_android="$B/llvm-ar" \
cargo clippy --target aarch64-linux-android --features mobile -- -D warnings
```

Host toolchains per platform (macOS/Linux/Windows): Xcode + command-line tools
for Apple targets; MSVC toolchain + Windows SDK for Windows; Wayland/X11,
xkbcommon and fontconfig dev packages for Linux; JDK 17+, the Android SDK/NDK
and `cargo-ndk` for Android; LLVM for the wasm32 C toolchain. Nothing is
version-pinned in prose — the manifests and toolchain files declare what
builds (see the platform table in AGENTS.md).

Android's SDK levels are declared by the `native/android` Gradle host —
JDK 17+, the Android SDK/NDK and `cargo-ndk` are the prerequisites, and the
APK assembly runs through that Gradle host. Native modules inherit the app's
compile SDK; device-side
prerequisites and the tandem harness are in
[docs/device-testing.md](docs/device-testing.md#prerequisites). The macOS run
also installs `xtool` and offers its Apple ID login. Android requests
ACCESS_LOCAL_NETWORK at runtime for LAN peers. Denial leaves local-first
startup available while Android blocks LAN traffic; gpui's iOS backend owns
the scene lifecycle and URL hooks; macOS retains AppKit lifecycle handling.
SDK compilation is not device certification: cold/warm links, permissions,
camera cancellation, and background/foreground recovery still require
target-device checks in [docs/debugging.md](docs/debugging.md).

`.github/workflows/quality.yml` checks formatting, desktop/Web Clippy, Swift, and
the landing build on PRs. `release.yml` replaces placeholder PGO with bounded
optimized builds and non-executing binary/architecture checks; it uploads
**verification artifacts only**, without a production pepper, signing, or
automatic publication. The release job records the pepper state in its build
metadata: an artifact built without `OO_APP_PEPPER` is unprovisioned and cannot
hash a password, while a supplied pepper is validated as base64 before the
build proceeds. Linux/macOS raw-binary archives are not asset-complete
application bundles; with gpui the direct cargo exe is the ship artifact on
every OS (Windows resources embed via the `windows-manifest` feature). The
macOS CI runner image is a preview one,
not the locally verified GA Xcode build; the label each job uses is in
`.github/workflows/`.

Use `cargo fmt --all -- --check` for a non-mutating formatting check. There is
no Tailwind pipeline anymore: gpui styles in Rust (`src/ui/theme.rs` + widget
builders), and the last generated `assets/tailwind.css` dies with the Dioxus
cutover.

### Brand Assets

The logo is pixel art — the nerd mascot from the 404 page, whose round
glasses spell "oo" (two lenses, two devices, one identity), in the palette of
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
| `assets/icons/ios/AppIcon*.png` | iOS launcher icons — `CFBundleIcons` in `native/apple/Info.ios.plist`; copied into the `.app` by `scripts/ios-device-deploy.sh` |
| `assets/icons/icon.icns` | macOS Dock icon — `CFBundleIconFile` in `native/apple/Info.macos.plist`; copied into the bundle by `scripts/fix-bundle-case.sh` |
| `assets/icons/android/*.xml` | Android adaptive-icon drawables — injected by `scripts/android-launcher-icons.sh` |
| `landing/public/{logo.svg,favicon.ico,apple-touch-icon.png}` | landing site |

The copy steps exist because gpui embeds resources through its own asset
pipeline; the mobile bundlers (xtool/gradle) consume the generated icons from
`assets/icons/` directly.

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
`VeilidConfig::new("oo", "debanjanbasu", "com", storage_root, None)`, which
places `protected_store/`, `table_store/`, and `block_store/` below it. Web
passes no filesystem root, keeping Veilid's empty directory settings and
`APP_ID`-namespaced IndexedDB/protected-store data. Android prefers the
Activity's `getFilesDir()`; when unavailable it searches `HOME` then the
current directory for the app's `files/` directory, finally using
`./com.debanjanbasu.oo/files/.veilid/`.

| Platform | Veilid storage root |
|---|---|
| macOS | `$XDG_DATA_HOME/Library/Application Support/com.debanjanbasu.oo/.veilid/` or `$HOME/Library/Application Support/com.debanjanbasu.oo/.veilid/` |
| iOS | `$CFFIXED_USER_HOME/Library/Application Support/com.debanjanbasu.oo/.veilid/` (falls back to the current directory, then `/private/var/mobile`) |
| Windows | `%APPDATA%\com.debanjanbasu.oo\.veilid\` or `%LOCALAPPDATA%\com.debanjanbasu.oo\.veilid\` |
| Linux/Unix | `$XDG_DATA_HOME/com.debanjanbasu.oo/.veilid/` or `$HOME/.local/share/com.debanjanbasu.oo/.veilid/` |
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
random account ID -> Argon2(password, salt = account ID) -> account-bound DHT secret (pepper as derive_shared_secret key) -> encrypted keypair -> local save
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

- Pairing accepts `oo://pair?data=...`, `https://oo-app.pages.dev/p/{payload}`, or raw payload; v3 pairing PINs are exactly 12 digits and expire after 5 minutes.
- Contact add accepts: `oo:username#dht_key`, `https://oo-app.pages.dev/c/{username}/{dht_key}`, encoded deep link, or `username|dht_key`.
- Pairing and share actions trigger reactively once valid inputs are provided; superseded async attempts are discarded.

---

## DHT Record Structure

The authoritative identity and conversation layouts, reserved subkeys, encryption/compression modes, ownership, and archive behavior are in [docs/dht-architecture.md](docs/dht-architecture.md). Keep this README as a navigation index, not a second schema specification.

---

## Security Model

| Mechanism | Detail |
|---|---|
| Password hashing | Argon2, salted with the 16-byte account ID |
| DHT AEAD key | Password hash + random account ID under a versioned domain separator |
| Storage encryption | AEAD on DHT/local persisted payloads (local rows encrypted by Veilid's TableDB under a device key in the OS protected store; insecure keyring in debug and e2e builds, browser storage on web) |
| Identity key material | Account-bound encrypted keypair blob on identity subkey 3 |
| Password rotation | Re-encrypt identity-sensitive subkeys |
| PIN | Argon2 hash (local + synced security state) |
| Biometrics | Platform-native auth bridges |
| Pairing | v3 QR payload + exact 12-digit, five-minute PIN + account password; remember-me wrap key remains device-local |
| Backups | v3 AEAD-header-bound format using the account-bound DHT key; older formats are not imported |
| Deep links | `oo://` + universal links under `https://oo-app.pages.dev` |

---

## Compression
Structured, Plain DHT transport frames, and local TableDB rows use zstd level 12 (`ZSTD_LEVEL = 12`; level 22 was dropped because web runs the codec inline on the UI thread). See [docs/compression.md](docs/compression.md) for framing, dictionary lifecycle, and local-storage policy.

---

## Global State Catalog

This is the single catalog of process state and ownership. `GlobalSignal`s must be accessed from Dioxus runtime tasks, not `tokio::spawn`. Session-scoped writes pair `UserCtx` with a `SessionGuard` and hold the appropriate account commit lock through local commit; `sync::mutate` locks persisted-account ownership before its account lock. Domain-local statics and caches remain with their owning modules.

| State | Owner | Notes |
|---|---|---|
| `nav_bar_items()`, `last_active_route()`, `theme()`, `network_status()`, `conversation_meta()` | `src/global_state.rs` | Accessors for core app signals |
| `SessionPhaseGlobal`, `RevisionBus`, `RouteStateGlobal` | `src/ui/state.rs`, `src/ui/router.rs` | gpui-side state (migration): session phase, per-model revision bus, router state — replaces the signals above as screens cut over ([docs/migration-gpui-ce.md](docs/migration-gpui-ce.md)) |
| `SESSION` | `src/session/phase.rs` | Auth state machine and current user source of truth |
| `ROUTE_RESTORED` | `src/router/helpers.rs` | Admission-scoped local hydration and route persistence gate |
| `PENDING_ACTION` | `src/platform/deep_link/mod.rs` | Typed pending action; native bridges retain URLs before runtime startup |
| `CrdtModel::revision_signal()` | `src/storage/store_trait/mod.rs` | Per-model `GlobalSignal<u64>` revision notifications after successful local commits |
| `SNAPSHOT_REVISION` | `src/storage/snapshots/mod.rs` | Invalidates snapshot-history resources |
| `DEEP_LINK_TOAST` | `src/components/overlays/deep_link_toast.rs` | Deep-link toast state |
| `SHOW_ACTIVITY_MODAL` | `src/components/overlays/mod.rs` | Overlay-local UI signal |
| `SYNC_STATUS`, `TRAINING_STATUS`, `UPGRADE_STATUS`, `HYDRATION_STATUS`, `PAIRING_SYNC_STATUS`, `BACKGROUND_ERRORS` | `src/storage/mod.rs`, `src/storage/background_errors.rs` | Operational status signals |
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

## App Pepper (`OO_APP_PEPPER`)

Auth key derivation uses an app-level pepper compiled via `option_env!("OO_APP_PEPPER")`. It is the `derive_shared_secret` key over the credential hash — never the Argon2 salt of a new account. The Argon2 salt is the 16-byte account ID, public by design, which is what makes pairing reproducible; so equal passwords no longer produce equal stored hashes either.

The DHT `SharedSecret` derivation also includes the random 16-byte account ID in a versioned, domain-separated input, so equal passwords do not produce the same DHT key.

```bash
# .env (or environment) must contain OO_APP_PEPPER=<secret>
cargo build --release --features desktop
```

Never commit the pepper. The one exception is deliberate and marked as such: `e2e.yml`
carries a fallback value for harness builds (`--features e2e-seams`, never distributed, and
`src/helpers/encryption.rs` only refuses an *empty* value), so a dispatch works without any
repository configuration. It is overridden by `secrets.OO_APP_PEPPER`, then
`vars.OO_APP_PEPPER`. Both peers of a harness run come from a single build, so any one
consistent value lets them interoperate; a distributed build must still be given a real
pepper, and an artifact built without one is unprovisioned.

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
