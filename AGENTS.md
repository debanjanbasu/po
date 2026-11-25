**Latest version, always — for every tool, dependency, and library.** This applies
to what is already in the tree, not only to new installs:

- Adding a dependency: search the internet for the current release first. Never
  rely on a stale snippet or outdated documentation.
- Touching anything versioned (a manifest, a workflow pin, a toolchain file):
  check it against the current release and bump it in the same change. A bump
  that needs a companion step — Playwright's browser download, a refreshed lock
  file — is part of that change, not a follow-up.
- Never restate a version number in prose or docs. Read the file that declares it
  (see [Project and Component Maps](#project-and-component-maps)); a copy in prose
  only drifts.

---

# Documentation & Code Lookup Preference

**ALWAYS prefer checking LOCAL source code first** when understanding APIs or behavior. If a crate is already in `Cargo.toml`, its source is available locally — **never** search the web or fetch from GitHub when you can read the actual code from the cargo cache.

## Local Source Lookup (MANDATORY)

1. **Git dependencies** (e.g., `dioxus`, `veilid-core`, `recapn`): Source lives in `~/.cargo/git/checkouts/`. Find the correct checkout dir and revision:
   ```
   # Find the checkout directory
   find ~/.cargo/git/checkouts -maxdepth 2 -name "<crate>*" -type d
   # Find the active revision from Cargo.lock
   grep -A3 'name = "<crate>"' Cargo.lock
   ```
2. **crates.io dependencies**: Source lives in `~/.cargo/registry/src/`. Find by crate name + version:
   ```
   find ~/.cargo/registry/src -maxdepth 1 -name "<crate>-<version>" -type d
   ```
3. **Only fall back to web** if the crate is NOT in `Cargo.toml` / `Cargo.lock` (i.e., you're evaluating a new dependency).

When docs conflict with code, **code wins**. Local source is always more accurate than web documentation.

# Documentation Sync Policy

**After every code change, update all affected documentation before committing.** Documentation drift is a bug.

Canonical documentation owners:
* **`README.md`** — user-facing overview, Build Verification, architecture navigation, the single Global State Catalog, project structure, and brand asset map.
* **`AGENTS.md`** — execution rules, task routing, and coding policies.
* **`docs/`** — authoritative deep dives for their listed domains.

What triggers a doc update:
* Adding/removing/renaming a route, feature, or component
* Changing default behavior (e.g., default landing page, auth flow)
* Adding/removing a global signal or DHT subkey
* Changing the sync pipeline, compression, or security model
* Adding/modifying a coding convention or lint rule

**Rule:** Update the canonical owning document and affected links when behavior or policy changes; update README/AGENTS only when their own content changes. Keep duplicate descriptions out of secondary docs.

# Agent Fast Start

Use this file as the **execution ruleset**. Use linked docs in `docs/` for deep architecture details.

## Task Routing Matrix (read before editing)

| If your task touches... | Read first |
|---|---|
| DHT subkeys, child records, `FeatureId`, SMPL record layout | [docs/dht-architecture.md](docs/dht-architecture.md) |
| Pending operations, drain loop, retries/DLQ, handler dispatch | [docs/sync-pipeline.md](docs/sync-pipeline.md) |
| Watch lifecycle, `ValueChange` handling, conversation watch manager | [docs/watch-system.md](docs/watch-system.md) |
| `app_message`, typing/read signals, route blobs, peer route resolution | [docs/peer-signaling.md](docs/peer-signaling.md) |
| Session guards, spawn patterns, fan-out policy (`map_concurrent`), signal safety, lock usage | [docs/concurrency.md](docs/concurrency.md) |
| zstd dictionary training/recompression/versioning | [docs/compression.md](docs/compression.md) |
| Tailwind/DaisyUI/CSS-first conventions and page sizing | [docs/ui-conventions.md](docs/ui-conventions.md) |
|Debugging a running app, live logs, cross-device (pairing) bugs, error triage|[docs/debugging.md](docs/debugging.md)|
|Tandem/e2e device testing, pairing or sync verification on real hardware|[docs/device-testing.md](docs/device-testing.md)|

# Project & Platform Rules

See [README.md](README.md) for the user-facing overview, architecture stack, and data directories; [docs/dht-architecture.md](docs/dht-architecture.md) is authoritative for DHT record layouts and wire/storage behavior.

**Veilid coding rules:** Follow the per-subkey encryption, compression, ownership, and allocation rules in [docs/dht-architecture.md](docs/dht-architecture.md); do not assume all slots share one format. Features resolve child record keys via `user::record::resolve_child_key_for_id()`. `FeatureId` is a string-based newtype (`struct FeatureId(pub Arc<str>)`) — not an enum. Never allocate a per-device numeric dictionary version: trained dictionaries are identified by their content digest (`b3:<digest>`), and retired subkey 7 remains unused.

## Project and Component Maps
Use [README.md § Project Structure](README.md#project-structure) as the authoritative directory and component map.

Brand asset generation and output maps live in [README.md § Brand Assets](README.md#brand-assets); edit the generator, not generated files.


Platform metadata (display name, bundle ID `com.debanjanbasu.po`, permissions) lives in `native/apple/Info.ios.plist`, `native/apple/Info.macos.plist`, and `native/android/AndroidManifest.app.xml`, wired via `[application]` in `Dioxus.toml`. Entitlements have no file here: dx's signing path reads none of `[application]`'s entitlements keys, so declare them under `[ios.entitlements]`/`[macos.entitlements]` if the app ever needs one. The visible app name is lowercase `po` on every platform — never Pascal-case it. Keep Apple `CFBundleShortVersionString` and `CFBundleVersion` in sync with `Cargo.toml`.

Version numbers are never restated in this file — a prose copy drifts from the
config that actually builds. Read the source instead:

| What | Where it is declared |
|---|---|
| Android SDK levels (min/target/compile) | `[android]` in `Dioxus.toml` |
| Apple deployment targets | `.cargo/config.toml`, `native/apple/Info.*.plist` |
| Rust channel and installed targets | `rust-toolchain.toml` |
| Node version | `.nvmrc`, `engines.node` in `package.json` |
| Crate versions, `dioxus`/`manganis`/`veilid-core` revisions | `Cargo.toml`, `Cargo.lock` |
| Which CI runner each job uses | `.github/workflows/*.yml` |

Keep each of those at its latest working release (see the rule at the top of this
file). A pin that only exists because nobody re-checked it is drift, not a
compatibility target.
Native architecture policy: Linux and Windows support x86_64 and ARM64;
macOS, iOS, and Android are ARM64-only. Name the image generation explicitly
(`ubuntu-26.04-arm`, `xcode-27`) and bump it deliberately; never `-latest`. The
floating alias lags the release the job builds against — `ubuntu-latest` still
resolves to 24.04 — and cannot express the architecture a job needs, while an
Apple job's image must carry the Xcode generation the app's deployment target
requires. Android compiles against the newest installed platform and
build-tools for the `[android]` SDK level; native modules inherit `compileSdk`,
`compileSdkMinor`, `buildToolsVersion`, and `minSdk` from `:app`. Native Android modules
rely on AGP's built-in Kotlin; do not apply `org.jetbrains.kotlin.android` or
set `kotlinOptions`. Use `jniLibs.useLegacyPackaging` for native libraries; do
not set `android:extractNativeLibs` in manifests.
Android requests ACCESS_LOCAL_NETWORK for LAN peers; denial must not gate Internet
network attachment. iOS uses Tao's scene lifecycle:
construct its event loop before installing the launch bridge, then run that same
loop. `DeepLinkPlugin.swift` hooks only Tao's app/scene delegates and captures
cold/warm URLs and user activities. `ScenePresentation.swift` supplies foreground
key-window presenters for sharing and QR capture; never select a background scene
as a fallback. Camera start/stop stays serialized off the main actor.

Android QR scanning uses Google Play Services Code Scanner, which handles camera access without an app `CAMERA` permission. Add that permission only if a direct camera API is introduced.


## Import Paths

All internal imports use canonical paths defined by `pub use` statements in each module. Use: `session::`, `storage::`, `platform::`, `user::identity_context`, `user::sync_models`.

## Node Tooling

- Use the root `package.json` and `package-lock.json` for all Node tooling; run `npm ci` from the repository root. The Astro site uses the root scripts `npm run landing:dev`, `npm run landing:build`, and `npm run landing:preview`.
- Do not pin an exact Node release. `.nvmrc` contains `node` (nvm's latest
  alias); run `nvm install` locally to refresh it. CI uses `latest`, and
  Cloudflare Pages reads `.nvmrc`. Keep `engines.node` in `package.json` at the
  minimum the native TypeScript scripts need — that file is the only place the
  floor is written.
- Keep directly executed `.ts` files within Node's erasable TypeScript syntax; avoid enums, namespaces, parameter properties, decorators, or TypeScript features that require runtime transforms.

### Linting (all JS/TS)

`npm run lint` (oxlint + `oxlint-tsgolint`, config in `.oxlintrc.json`) must pass
for every `.ts`, `.tsx`, `.js`, `.mjs` and `.astro` file — the harness, `scripts/`
and the landing page alike. `npm run lint:fix` applies the mechanical fixes.

- **Fix the code, never silence the rule.** A new `oxlint-disable` needs a
  one-line reason naming the requirement it protects, and it must be as narrow
  as the file/line allows. Blanket `allow`s in the config are not a fix.
- Type information comes from `tsgolint` (a Go implementation using
  `typescript-go`), so the lint needs no build step and works on the latest
  TypeScript. `typescript` is a dependency for that reason — do not pin it back.
- Categories enabled are `correctness`, `suspicious`, `pedantic` and `perf`.
  `style` and `restriction` are deliberately not enabled: they are opinionated
  formatting/feature-banning sets (`sort-keys`, `capitalized-comments`,
  `no-ternary`, `no-optional-chaining`, `no-async-await`) that contradict this
  codebase's conventions, and enabling them would mean hundreds of disables.
- `.astro` frontmatter is linted; unused-variable analysis is suppressed there on
  purpose, because a frontmatter binding may only be referenced from the
  template. `.astro` **type checking** is not available yet: `astro check`
  refuses TypeScript 7. Revisit when Astro supports it.
- Two rules are scoped off by the config's `overrides` (oxlint rejects comments
  in that file, so the reasons live here):
  - `typescript/prefer-readonly-parameter-types` for `test/**` and `scripts/**`.
    Device surfaces, run contexts, capability maps and operation lists are
    mutable handles whose mutation happens through methods; `Readonly<>` on those
    parameters would satisfy the rule without preventing a single write. It
    stays enabled for `landing/`.
  - `eslint/max-lines-per-function` for `test/scenarios/**`. A scenario's `run`
    is its linear script in step order and `ScenarioEntry` needs one entry point
    per scenario; splitting it would hide the sequence the file exists to show.

## Module Boundary Rules

### `helpers/` boundary (prevent catch-all growth)

- `src/helpers/` holds cross-domain utilities — today `async_utils`, `capnp`, `concurrency` (the single `map_concurrent` fan-out), `encryption`, `framing` (the single length-prefixed binary framing), and `store_lock`; platform bridges live in `src/platform/`, storage/sync infrastructure in `src/storage/`, and session management in `src/session/`.
- If a helper is primarily used by one domain, keep it in that domain (`src/chat/`, `src/user/`, `src/compression/`). User-specific validation, crypto, and context live in `src/user/`.
- Prefer narrow modules with explicit purpose over broad grab-bag helper files.

### `user/` internal service boundaries

- Keep internal boundaries explicit to reduce fan-out:
  - **Crypto ops** (`biometric/`, `pairing_crypto.rs`): biometric check/enroll, pairing payload encode/decode; session transitions delegated to `session::phase`
  - **Session/auth ops** (`auth/`): login state, pending user, session transitions
  - **Store/sync ops** (`store/mod.rs`): local persistence, pending-op enqueue, hydration handoff
  - **Identity record ops** (`record/`, `sync_ops.rs`): DHT identity record CRUD, child record management, identity sync
  - **Validation** (`validation.rs`): username/password/PIN/DOB validators, pairing validation
- Cross-submodule calls should flow through these boundaries, not via ad-hoc direct calls between unrelated files.
- Orchestrator functions are coordination points only; avoid embedding low-level crypto/storage details directly in orchestration layers.

# Rust Coding Standards

## Module Export Conventions

Prefer **retail exports** (`mod` + `pub use`) over **wholesale exports** (`pub mod`):

```rust
// Preferred: module private, items selectively exposed
mod internal_module;
pub use internal_module::{Item1, Item2};

// Avoid: both pub mod AND pub use (redundant)
pub mod internal_module;
pub use internal_module::{Item1, Item2};  // redundant
```

**Rule**: Hide internal module structure. Only use `pub mod` when the module path itself is the intended user-facing API (e.g., `auth::login_signup::LoginSignup`).

**Canonical import paths** are defined in each module's `pub use` statements. Consumers should import from the canonical path (e.g., `components::auth::LatestAttemptController`) rather than the internal module path.

* Hoist all `use` statements to the top of the file. No nested full paths (e.g., `std::mem::replace` or `crate::foo::bar()`) allowed inside functions or components.
* Merge `use` statements from the same root using curly braces (e.g., `use std::{iter::repeat_with, mem};`).
* Minimize variable declarations. Avoid declaring additional variables unless absolutely necessary.
* Prefer concise, idiomatic Rust.
* Leverage modern Dioxus error handling: Use `?` with `Ok(())` in closures (event handlers, async tasks) and `?` in component bodies to bubble errors to the nearest `ErrorBoundary`. Prefer a global `ErrorBoundary` for generic error handling to keep child components clean.
* **Inline error signals for user-facing validation**: For predictable failures (wrong password, no QR found, invalid input), use a local `use_signal` with inline `text-error` display — do not bubble these to `ErrorBoundary`. Reserve `ErrorBoundary` / `throw_error` for unexpected or unrecoverable errors only.
* **`ErrorBoundary` never sees a panic**: `panic = "immediate-abort"` (with `panic_abort` std, both profiles) aborts before the vdom's `catch_unwind` can turn a panic into the `CapturedPanic` a boundary consumes. Only the `Result`/`?`/`throw_error` path above reaches a boundary.
* **Interactive internal failures** (submit/click/typing flows) should surface immediately near the active input/control with a user-safe message; keep full technical detail in logs.
* **Background non-fatal failures** should use `storage::background_errors::{spawn_report, report_nonfatal}` so they become visible in the Activity modal (`BACKGROUND_ERRORS`) and not logs-only.
* **`thiserror` vs `anyhow` split**: Use `thiserror` enums in domain modules when callers branch on error kinds (validation/auth/pairing/schema-fallback paths). Use `anyhow` at orchestration boundaries where errors are only propagated with context.
* **No error-string control flow**: Do not branch on `e.to_string().contains(...)`. Prefer matching typed error variants; convert typed errors to user-safe strings only at UI boundaries.

## Clippy Strictness

This project enforces very strict clippy lints. All new code must comply:

* `nursery` and `pedantic` = **deny** (priority -1)
* `unwrap_used`, `expect_used`, `panic`, `string_slice` = **deny**
* `unsafe_code` = **deny** (Rust-level)
* Allowed exceptions live in `[lints.rust]`/`[lints.clippy]` in `Cargo.toml`, and
  each carries its measured rationale as a comment beside it — read there, never
  a copy. They are crate-wide because the alternative is not narrower: the two
  macro-generated-code lints fire on derive output, `manganis` drops attributes
  written on `extern` declarations, and the rest describe chosen architecture
  (single-threaded runtime, Veilid's future sizes). Fixing them properly means
  patching the fork, not adding inline `#[allow]`s here.

## Verification Policy

Do not add or run tests in this pre-release codebase. Use the verification commands in [README.md § Build Verification](README.md#build-verification) and [README.md § Release Baselines](README.md#release-baselines); validate platform-truth behavior on target devices before release. Do not claim a change is verified without evidence.

Do not use `--locked` for local installs or builds — the repo tracks bleeding-edge git dependencies (`dioxus`/`veilid` forks) where the lock should be refreshed, not enforced. CI workflows are the exception: they pin `--locked` deliberately so a build is reproducible against the committed lockfile, and they fail the job instead of refreshing it.

Prevent regressions on hot paths (sync queue, watchers, compression, chat open/send).

## Import Hoisting Convention
**All `crate::`, `std::`, `dioxus::`, `tokio::` paths referenced inside function/component bodies must be hoisted to the file's top-level `use` block.** Only these may keep fully-qualified paths:
- Macro bodies (`$crate::`, `#[autosurgeon(with = "...")]`)
- `cfg`-gated module-scope uses
- `pub(in crate::...)` visibility syntax
- `crate::nonfatal!` invocations (macro)
- Re-export definitions (`pub use crate::...`)

Replace hand-rolled Poison recovery with the idiomatic helper:
```rust
// Before
lock().unwrap_or_else(std::sync::PoisonError::into_inner)
// After
helpers::store_lock::recover_poison(lock())
```

## Dead Code Policy

When dead code is discovered during code review:

1. **Wire it up** — If the code provides utility (batch operations, optimizations), find places to use it
2. **Delete it** — If the code was added prematurely for optimization that isn't needed, remove it
3. **Never use `#[allow(dead_code)]`** — This hides the problem instead of solving it

**Rationale**: Dead code increases maintenance burden and cognitive load. Either use it or delete it.

## Non-Fatal Error Handling

For secondary operations that should not fail the primary flow, try to see if we can bubble up the error first. Otherwise, use explicit logging instead of silent `let _ =`:

```rust
if let Err(e) = result {
    warn!("Secondary operation failed: {e}");
}
```

Apply to: `save_local`, `persist_and_sync`, `open_dht_record`, `watch_dht_values`, `move_to_dead_letter`, and other non-critical ops. Retain `let _ =` only for true fire-and-forget cleanup (shutdown, delete cascade, platform FFI).

## Cross-Platform Time

`std::time::Instant` and `std::time::SystemTime::now()` **panic on WASM** (`wasm32-unknown-unknown`). Use `now_epoch_millis()`, `now_epoch_secs()`, `sleep_ms()`, and `timeout()` from `helpers/async_utils.rs` instead (see [docs/concurrency.md § Cross-Platform Time](docs/concurrency.md#cross-platform-time)). Only use `Instant` or `SystemTime::now()` inside `#[cfg(not(target_arch = "wasm32"))]` blocks.

# Dioxus Dependency
Key deps are git-based: `debanjanbasu/dioxus` `main` provides `dioxus` (with `router`) and `manganis`; `debanjanbasu/veilid` `main` provides `veilid-core`; `recapn` remains upstream. See `Cargo.toml` and `Cargo.lock` for exact revisions.

The fork renamed the VirtualDOM crate `dioxus-core` to `dioxus-vdom`; the
`dioxus-core` name is now a prelude facade. Import vdom items (`spawn_forever`,
`Task`, …) from `dioxus::core::…`, not `dioxus::dioxus_core`.

Rust runs on the rolling channel in `rust-toolchain.toml`; run `rustup update`
locally to refresh it. Verify `dx` against the Dioxus/Manganis Git revision in
`Cargo.lock`, not its version string. CI uses `cargo fmt --all -- --check`; the
locked `dx fmt --check` is mutating and is not
an assurance check. Release verification artifacts are unprovisioned, not
publishable releases; production pepper/signing belongs to a separate trusted
flow. CPU baselines in `.cargo/config.toml` apply to local release bundles too.

Platform features: no default (requires `--platform web`, `--platform desktop`, or
`--platform mobile`). `web` uses Dioxus web and Veilid `default-wasm`; `desktop`
uses Dioxus desktop; `mobile` uses Dioxus mobile. The native target dependency
enables Veilid `enable-crypto-vld0`, `rt-tokio`, and `backtrace` (including Tokio
for featureless rust-analyzer/Cargo tooling); this does not select a Dioxus
renderer or make featureless app builds supported.
Optional network features stay explicit: `aimd-dht` enables Veilid's experimental adaptive DHT byte gate, while `veilid-stats` emits periodic Veilid NDJSON performance counters. Neither belongs in normal builds without a measured reason.

This project is client-side only. Do not introduce SSR/fullstack/server-function patterns unless explicitly requested.

# Dioxus Practices

Follow current Dioxus patterns used in docs and repo examples:

* Prefer `use_signal`, `use_memo`, `use_effect`, and `use_resource` for state and async data.
* Keep `#[component]` functions simple and use `?` in component bodies when falling through to an `ErrorBoundary`.
* Use `asset!` and `document::Stylesheet` for styles and static assets.
* For async data in this client-only app, prefer `use_resource` and browser/client APIs.

---

# Form Input Patterns

Use the shared form module (`components::shared::form`) for validated text inputs. All form inputs MUST follow this pattern unless the field has complex rendering requirements (join layouts, inline buttons) that don't fit the render functions.

### Hook-first pattern

```rust
use components::shared::{use_form_field_manual, render_validated_password};

let handle = use_form_field_manual("", true, |v| {
    if validate(v) { None } else { Some("error hint") }
});
```

`FormFieldHandle` provides: `value: Signal<String>`, `error: Memo<Option<&'static str>>`, `is_valid: Memo<bool>`, `is_dirty: Signal<bool>`. Derives `Copy` — a handle can be passed by value, and the render functions below borrow it instead (`&handle`).

The second argument is `required`; pass `true` for required fields.

Access via `(handle.value)()`, `(handle.is_valid)()`, `(handle.error)()`. Setters: `handle.value.set(...)`, `handle.is_dirty.set(...)`.

### Render functions

- `render_validated_pin(&handle, label, id)` — numeric PIN input with an accessible name and placeholder from `label`, the input's DOM `id` from `id` (`Option<&str>`), and inline hint text hidden until an error exists. The helper applies `PIN_MIN_LEN` internally; callers do not pass a minimum length.
- `render_validated_password(&handle, label, autocomplete, id)` — password input with an accessible name and placeholder from `label`, the input's DOM `id` from `id` (`Option<&str>`), `autocomplete` passed through, and inline hint text that stays hidden until an error exists
- `render_validated_date(handle, label, hint)` — date picker input that keeps its instructional hint row visible when there is no validation error

Password/PIN wrappers render a plain `input.input-sm.validator.w-full` plus an optional `p.validator-hint`. The date wrapper still uses the structured label shell because its existing profile flow still depends on the always-visible instructional copy.

### FormFeedback

Use `FormFeedback` component for form-level feedback (success/error):

```rust
FormFeedback { kind: FeedbackKind::Success, message: "Saved".into() }
```

For dense surfaces that should avoid the full DaisyUI alert banner, pass `compact: true` to render a small inline text row without the icon:

```rust
FormFeedback { kind: FeedbackKind::Success, message: "Saved".into(), compact: true }
```

### Validation rule

Validators return `Option<&'static str>` where `None` = valid and `Some("hint")` = the inline error text shown in `validator-hint`. Password/PIN wrappers suppress the row until a real error exists; the date wrapper keeps the idle instructional hint.

---

# Serialization with recapn (Cap'n Proto)

Schemas in `src/schemas/*.capnp`, compiled via `recapnc` in `build.rs`. Generated code included via `include!` from `OUT_DIR` in `src/models/mod.rs`.

Models use `pack`/`unpack` from `helpers/capnp.rs`. See `src/user/model.rs` for the canonical pattern. Use packed encoding for storage. Read text with `.get().as_str()?`, set with `.set_str()`.

---

# UI Conventions

See [docs/ui-conventions.md](docs/ui-conventions.md); preserve accessibility and follow its CSS-first and page-sizing conventions.

---

# Logging

Use Dioxus's built-in tracing facade (`dioxus::logger::tracing::{info, warn, error, debug}`) in application code. Native logging is initialized with `tracing-subscriber` in `main.rs`; Web uses Dioxus's logger. Use structured tracing rather than `println!`/`eprintln!` for diagnostics.

# Clone Avoidance Guidelines

**Prefer references over cloning unless ownership transfer is required.** Use `&str` over `String`, `&[u8]` over `Vec<u8>`. Use `Arc<str>` for frequently-cloned strings (usernames, IDs). `User::username()` returns `&str`; `User::dht_key_arc()` returns an owned `Arc<str>`. Runtime map keys (`CONVERSATION_META`, `ConversationManager`, `ROUTE_CACHE`, `change_dispatcher`) use `Arc<str>` — avoid `.to_string()` for map keys; prefer `Arc::from(s)` or `.into()`.

When cloning is appropriate: sending across async boundaries (`spawn`), releasing `GlobalSignal` read guards before `.await`, and serialization.

**Crate-specific patterns:**
* **veilid-core**: Use sync `CryptoSystemGuard` (via `crypto!` macro or `crypto_system!`) for AEAD, hashing, key generation — takes `&[u8]` not owned `Bytes`. Only use async guard for operations that may block.
* **bytes**: `Bytes::from(vec)` is zero-copy; `Bytes::copy_from_slice(&[u8])` copies. Prefer `Vec<u8>` → `.into()` when you already own the data.
* **autosurgeon**: Only supports `String` for `Reconcile`/`Hydrate` — cannot use `Arc<str>` in CRDT models.
* **zeroize**: Wrap password hashes in `Zeroizing<String>` for auto-zeroization on drop. Manual `.zeroize()` for `Vec<u8>` plaintext buffers.
* **session_crypto**: Uses `HashMap<Arc<str>, SessionCrypto>` — lookups accept `&str` via `Borrow<str>` impl.

## Schema Versions

Cap'n Proto messages carry schema versions; keep field ordinals sequential without holes or reuse. `CrdtModel::SCHEMA_VERSION` must be specified by each model. `storage/schema_version.rs` rejects newer stored versions rather than discarding them.

# Nightly Features

The rolling channel in `rust-toolchain.toml` allows unstable features. Required:
```rust
#![feature(try_blocks)]  // try { } blocks across all targets
```

# context-mode — MANDATORY routing rules

You have context-mode MCP tools available. These rules are NOT optional — they protect your context window from flooding. A single unrouted command can dump 56 KB into context and waste the entire session.

## Think in Code — MANDATORY

Analyze/count/filter/compare/search/parse/transform data: **write code** via `context-mode_ctx_execute(language, code)`, `console.log()` only the answer. Do NOT read raw data into context. PROGRAM the analysis, not COMPUTE it. Pure JavaScript — Node.js built-ins only (`fs`, `path`, `child_process`). `try/catch`, handle `null`/`undefined`. One script replaces ten tool calls.

## BLOCKED commands — do NOT attempt these

### curl / wget — BLOCKED
Any shell command containing `curl` or `wget` will be intercepted and blocked by the context-mode plugin. Do NOT retry.
Instead use:
- `context-mode_ctx_fetch_and_index(url, source)` to fetch and index web pages
- `context-mode_ctx_execute(language: "javascript", code: "const r = await fetch(...)")` to run HTTP calls in sandbox

### Inline HTTP — BLOCKED
Any shell command containing `fetch('http`, `requests.get(`, `requests.post(`, `http.get(`, or `http.request(` will be intercepted and blocked. Do NOT retry with shell.
Instead use:
- `context-mode_ctx_execute(language, code)` to run HTTP calls in sandbox — only stdout enters context

### Direct web fetching — BLOCKED
Do NOT use any direct URL fetching tool. Use the sandbox equivalent.
Instead use:
- `context-mode_ctx_fetch_and_index(url, source)` then `context-mode_ctx_search(queries)` to query the indexed content

## REDIRECTED tools — use sandbox equivalents

### Shell (>20 lines output)
Shell is ONLY for: `git`, `mkdir`, `rm`, `mv`, `cd`, `ls`, `npm install`, `pip install`, and other short-output commands.
For everything else, use:
- `context-mode_ctx_batch_execute(commands, queries)` — run multiple commands + search in ONE call
- `context-mode_ctx_execute(language: "javascript", code: "...")` — run in sandbox, only stdout enters context

### File reading (for analysis)
If you are reading a file to **edit** it → reading is correct (edit needs content in context).
If you are reading to **analyze, explore, or summarize** → use `context-mode_ctx_execute_file(path, language, code)` instead. Only your printed summary enters context.

### grep / search (large results)
Search results can flood context. Use `context-mode_ctx_execute(language: "javascript", code: "...")` to run searches in sandbox. Only your printed summary enters context.

## Tool selection hierarchy

0. **MEMORY**: `context-mode_ctx_search(sort: "timeline")` — after resume, check prior context before asking user.
1. **GATHER**: `context-mode_ctx_batch_execute(commands, queries)` — Primary tool. Runs all commands, auto-indexes, returns search. ONE call replaces 30+ individual calls.
2. **FOLLOW-UP**: `context-mode_ctx_search(queries: ["q1", "q2", ...])` — Query indexed content. Pass ALL questions as array in ONE call.
3. **PROCESSING**: `context-mode_ctx_execute(language, code)` | `context-mode_ctx_execute_file(path, language, code)` — Sandbox execution. Only stdout enters context.
4. **WEB**: `context-mode_ctx_fetch_and_index(url, source)` then `context-mode_ctx_search(queries)` — Fetch, chunk, index, query. Raw HTML never enters context.
5. **INDEX**: `context-mode_ctx_index(content, source)` — Store content in FTS5 knowledge base for later search.

## Parallel I/O batches

For multi-URL fetches or multi-API calls, **always** include `concurrency: N` (1-8):
- `context-mode_ctx_batch_execute(commands: [3+ network commands], concurrency: 5)` — gh, curl, dig, docker inspect, multi-region queries
- `context-mode_ctx_fetch_and_index(requests: [{url, source}, ...], concurrency: 5)` — multi-URL batch fetch

**Use concurrency 4-8** for I/O-bound work (network calls, API queries). **Keep concurrency 1** for CPU-bound (build, lint) or commands sharing state (ports, lock files, same-repo writes). GitHub API rate-limit: cap at 4 for `gh` calls.

## Output constraints

- Keep responses under 500 words.
- Write artifacts (code, configs, PRDs) to FILES — never return them as inline text. Return only: file path + 1-line description.
- When indexing content, use descriptive source labels so others can `search(source: "label")` later.

## Session Continuity

Skills, roles, and decisions persist for the entire session. Do not abandon them as the conversation grows.

On resume, search BEFORE asking the user — session history is persistent and searchable:

| Need | Command |
|------|---------|
| What did we decide? | `context-mode_ctx_search(queries: ["decision"], source: "decision", sort: "timeline")` |
| What constraints exist? | `context-mode_ctx_search(queries: ["constraint"], source: "constraint")` |

DO NOT ask "what were we working on?" — SEARCH FIRST. If search returns 0 results, proceed as a fresh session.

After /clear or /compact: knowledge base and session stats preserved. Use `ctx purge` to start fresh.

## ctx commands

| Command | Action |
|---------|--------|
| `ctx stats` | Call the `stats` MCP tool and display the full output verbatim |
| `ctx doctor` | Call the `doctor` MCP tool, run the returned shell command, display as checklist |
| `ctx upgrade` | Call the `upgrade` MCP tool, run the returned shell command, display as checklist |
| `ctx purge` | Call the `purge` MCP tool with confirm: true. Wipes knowledge base permanently. |
