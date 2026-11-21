# Dioxus → gpui-ce Migration Plan

Plan to replace Dioxus with [gpui-ce](https://github.com/debanjanbasu/gpui-ce) as the app framework on every target the app ships (macOS, Windows, Linux, iOS, Android, web). Analysis is against the fork's `main` at planning time; the dependency tracks that branch by git, like `dioxus` and `veilid-core` do today.

Scope of what changes vs what survives:

- **Unchanged:** the entire domain stack — Veilid (`src/veilid`, `veilid-core`), Automerge/autosurgeon storage, recapn wire schemas, sync pipeline, watch system, compression, session/epoch guards, crypto, DHT record layouts. gpui-ce is a UI-framework swap, not a rewrite of the app.
- **Rewritten:** `src/components`, `src/router`, `src/app` (~11.8k LOC), the `main.rs` launch edge, the styling system (Tailwind/daisyUI CSS → Rust style builders), the build pipeline (dx → cargo/Trunk/cargo-ndk), and the web leg of the e2e harness.
- **License to improve:** the rewrite is not line-for-line. Every ported component may be simplified, redesigned, or optimized while its behavior, `e2e-seams` markers, and validation rules stay identical: collapse duplicated widget patterns into shared builders, drop dead view state, replace signal-fan-out gymnastics with direct entity observers, strengthen types (newtypes over `String` where the domain already has one), and delete anything the new model makes redundant.
- **Trail-gated (not dropped):** iOS — resolved: `gpui_ios` is vendored in the fork workspace and green (see §1). Android — resolved: `gpui_android` was written into the fork workspace for this migration (no upstream equivalent exists); it compiles 0 errors for `aarch64-linux-android` (see §1).

---

## 1. Platform scope (all five targets stay)

gpui runs the same views on every target this app ships; no target is dropped.

| Target | Backend crate | Windowing / GPU / text raster | Notes for this app |
|---|---|---|---|
| macOS | `gpui_macos` | AppKit / Metal / Core Text | Borderless chrome via `TitlebarOptions::appears_transparent` |
| Windows | `gpui_windows` | Win32 / Direct3D 11 / DirectWrite | `windows-manifest` feature on by default |
| Linux (+FreeBSD) | `gpui_linux` | Wayland or X11 / wgpu (Vulkan, OpenGL) / Swash | Layer-shell, AT-SPI a11y, XIM/text-input-v3 IME |
| iOS | `gpui_ios` — **in the fork workspace** (vendored + ported, fork rev `8552f16`) | UIKit / Metal / Core Text | iOS 15+; one UIWindowScene; `set_status_bar_style`; Touch/FaceID flows keep `platform/apple_ffi.rs`. Vendored per the "Path to mobile" decision below and ported to fork-core 0.4 APIs; `ios-sim`/`ios-device` compile 0 errors and the app's `gpui-mobile` selector boots via `gpui_platform::current_platform` |
| Android | `gpui_android` — **in the fork workspace** (written for this migration; no upstream equivalent exists) | NativeActivity / wgpu via `gpui_wgpu` (Vulkan, GLES fallback) / Swash | API 26+ (project floor is 37); COLRv1 emoji not rasterized — bundle a bitmap emoji font; JNI host stays in `platform/android_plugin.rs`; compiles 0 errors for `aarch64-linux-android` |
| Web | `gpui_web` | One canvas / WebGPU→WebGL2 / Swash | Trunk build; threaded workers need COOP/COEP; web-sys QR/share/storage modules unchanged |

Verified against upstream gpui-ce, Zed's repo, and the published crates: desktop and web backends are in-tree on fork `main`; the iOS backend was originally out-of-tree (Zed's `gpui_ios`; crates.io `gpui_ce_ios` at `^0.3.0`) and has since been vendored in. **Committed targets today: web + desktop + iOS.** Phase 0's mobile-backend verification is resolved: `gpui_ce_ios` 0.3.0 was vendored into the fork workspace (`crates/gpui_ios`), ported to fork-core 0.4, and compiles 0 errors on `ios-sim` and `ios-device` (fork rev `8552f16`, pushed to `main`); the app wires it through the new `gpui-mobile` feature selector. iOS joins every later phase. Android's `gpui_android` backend was written into the fork workspace (rev `ba2729f26`): NativeActivity host, `gpui_wgpu` renderer (Vulkan-first, GLES fallback), Parley/Swash text; 0 errors for `aarch64-linux-android`.

`Window::insets()` (safe_area + ime, logical px) and `AppLifecyclePhase` are implemented by the mobile backends — `src/platform/lifecycle.rs` background_flush/foreground_catchup ports onto `on_app_lifecycle`.

## 2. What gpui-ce provides (verified against the clone)

- **App model:** `gpui_platform::application().run(|cx: &mut App| …)` → `cx.open_window(WindowOptions, |_, cx| cx.new(MyView::new))`. Web: `gpui_platform::web_init()` + `application_with_web_backend(WebBackendPreference::Auto)`.
- **State:** `Entity<T>` + `impl Render for T` (`render(&mut self, &mut Window, &mut Context<Self>)`). Observers: `cx.observe` / `cx.subscribe` / `cx.observe_global`; mutation ends with `cx.notify()`. Globals: `trait Global` + `cx.set_global` / `cx.global::<G>()`. Element-local state: `window.use_state`. Stateless widgets: `RenderOnce`.
- **Async:** `cx.spawn` (foreground, `AsyncApp`), `cx.background_spawn` (pool), `Task<R>` cancellation-by-drop. `gpui_tokio` runs a real Tokio runtime in a `Global` and bridges `Tokio::spawn(cx, fut)` → GPUI `Task` — this is the bridge for every Veilid worker future.
- **Styling:** Tailwind-shaped fluent methods generated by macros (`flex()`, `flex_col()`, `items_center()`, `gap_2()`, `p_4()`, `text_sm()`, `bg()`, `text_color()`, `rounded_lg()`, `hover_style()`, `transitions()`, `backdrop_blur()`), driven by `StyleRefinement`. Colors via `rgb()/rgba()/hsla()`; appearance-aware palettes via the `Colors` pattern (`Colors::for_appearance(window)`).
- **Elements:** `div`, `svg`, `img`, `canvas` (custom drawing — QR render), virtualized `list`/`uniform_list` (chat list/conversation), `deferred`, `anchored` popups, `with_animation` (activity/toast motion). Text inputs/areas with IME, selection, undo, caret blink: `gpui_ce_elements::{editable_text::{text_input, text_area}, switch}`.
- **Windowing:** `TitlebarOptions { title, appears_transparent, traffic_light_position }` (custom `WindowChrome` port), `WindowKind::{PopUp, Dialog, AnchoredPopup, Floating}` (modals), `WindowDecorations` (Linux), bounds/activation/appearance observers (replaces `use_wry_event_handler`).
- **Assets:** `AssetSource` trait (`load`/`list`); web path is fetch or `include_bytes!`; fonts via `cx.text_system().add_fonts` (TTF/OTF — **no WOFF2**).
- **Web runtime:** one `<canvas>` per page, one top-level window, WebGPU→WebGL2 fallback, Trunk build/serve, threaded background executor only with `multithreaded` feature + cross-origin isolation (COOP/COEP headers — pattern in `crates/gpui_web/examples/hello_web/Trunk.toml` and its `.cargo/config.toml`).
- **Web limitations that hit this app:** no DOM/a11y tree (canvas-only rendering), no file pickers, no system notifications, no keychain, no screen capture; clipboard reads async; `open_url` works. Camera (QR scan), Web Share, localStorage are **not** GPUI services — the existing `web-sys` impls (`platform/qr_scanner/wasm_impl.rs`, `platform/share/wasm_impl.rs`, `platform/browser_storage.rs`) keep working unchanged.
- **Testing:** `test-support` feature, `TestApp` (gpui's own, distinct from this repo's Tandem harness), leak detection, bench support.

## 3. API mapping (the port is mechanical once per pattern)

| Dioxus (today) | gpui-ce (target) |
|---|---|
| `LaunchBuilder…launch(App)` per platform in `main.rs` | `gpui_platform::application().run(...)` + `cx.open_window(...)`; web gets `web_init()` + `application_with_web_backend`; iOS/Android keep their native hosts and call the same closure (iOS via `main`, Android via `android_main` → the mobile backend's run hook) |
| `#[component] fn X() -> Element` + `rsx!` | `struct X { … }` + `impl Render` (stateful) or `RenderOnce` fn (stateless leaf) |
| `Signal<T>` / `use_signal` | `Entity<T>` field mutated via `cx` + `cx.notify()`; element-local via `window.use_state` |
| `GlobalSignal` (33 across 13 files: `SESSION`, `THEME`, `NETWORK_STATUS`, `CONVERSATION_META`, `PENDING_INVITES`, store revisions…) | `Global` structs owning the same data; `cx.set_global` at boot; `cx.observe_global` for UI rows that watched them; keep `ReadableExt`-style helpers as plain methods so call sites barely change |
| `use_memo` (73) | cheap derivations computed in `render`; expensive ones cached in an entity refreshed by `cx.observe` on inputs |
| `use_effect` (40) | `cx.observe`/`cx.subscribe`/`cx.observe_global` registered in the owning entity's constructor |
| `use_resource` (18) | `cx.spawn` + stored `Task` handle (drop = cancel); re-run by triggering an `Entity` version counter observed like the signal deps were |
| `spawn` / `spawn_forever` / `spawn_session_scoped` (79 + send.rs) | `cx.spawn` / `cx.background_spawn`; session-scoped variants keep their `SessionGuard` logic, now spawning from an `Entity` context instead of a scope |
| `use_drop` (7: Veilid shutdown, watchers) | `Drop` on the entity + `cx.observe_release`; process-exit paths move to the `Application` run-closure tail |
| `dioxus_router` `Routable`/`#[nest]`/`#[layout]`/redirects | hand-rolled router: same `Route` enum + path strings, an `ActiveRoute` entity, a `nav(to)` helper, history/deep-link sync; `NavLayout` dock becomes the shell's render wrapper |
| `class: "flex gap-2 …"` (563 sites) | fluent style methods (`flex().gap_2()…`); one-time port with a lint banning `class:` reentry |
| Tailwind + daisyUI themes (`shared/theme.css`, `overrides.css`) | `ui::theme` module: the bumblebee warm palette + dark theme expressed as `Hsla` tokens in an appearance-aware `Colors` struct; `ui-conventions.md` §Theme tokens carries over as Rust constants |
| `document::Style/Link`, `asset!`, manganis | `AssetSource` + `include_bytes!`; static web shell files via Trunk `index.html` |
| `dioxus::logger` | `tracing-subscriber` everywhere (already used on native); wasm: console layer via `tracing-wasm` or `console_log` |
| `use_wry_event_handler`, tao builders (`platform/window.rs`) | `WindowOptions`/`TitlebarOptions` + GPUI window observers (`observe_window_bounds/activation/appearance`) |
| `ErrorBoundary` / `?` in components | match on `Result` in `render`; keep `report_nonfatal!` paths identical |
| `devtools` feature / dx hot-patch | gpui `inspector` feature + debug overlay; gpui-ce hot-patching (see its `docs/pages/hot-patching.typ`) evaluated in Phase 0 |
| e2e web surface: Playwright DOM (`#login-username-input`, `aria-label` fills, `textContent` asserts) | **Gone on canvas.** Web surface asserts move to the existing `event=` seam markers (`e2e-seams` feature) + pixel/screenshot checks; DOM-driving moves to gpui's own `TestApp` in Rust tests where possible |

## 4. Target tree

```
src/
  main.rs                  # entry per target (replaces dioxus launch edge)
  ui/
    mod.rs    app.rs       # root view, auth-phase machine (ex ActiveRouter)
    router.rs              # Route enum, nav(), deep-link + restore mapping
    theme.rs               # palette/appearance tokens (ex daisyUI themes)
    widgets/               # text fields, buttons, modal, toasts, dock, qr views
    screens/               # auth, onboarding, chat, settings, totp, placeholders
  (domain modules unchanged: chat/ user/ storage/ sync/ veilid/ models/ …)
```

`Cargo.toml` features become: `default = []`, `web`, `desktop`, `mobile`, `e2e-seams` (only `devtools` dies; `aimd-dht` and `veilid-stats` were removed with the dioxus pipeline they were tuned through). Web build: `trunk serve/build --release` with COOP/COEP; desktop: `cargo build` + existing bundling scripts (minus dx); mobile: cargo-ndk + `xtool` per the mobile backends' packaging flow.

## 5. Phases

> **Status (2026-11-20, CUTOVER COMPLETE):** Dioxus is fully removed.
>
> - The frozen tree (`src/app`, `src/components`, `src/router`), the
>   `dioxus`/`manganis` deps, `asset!` consts, the `devtools` feature,
>   `Dioxus.toml`, the Tailwind pipeline, and the dx tooling are all deleted.
> - The domain runs on dioxus-free primitives: `helpers::signals`
>   (`GlobalSignal` over `RwLock` with the same peek/read/write/with API),
>   `helpers::spawn`/`spawn_forever` over a dedicated single-thread
>   `LocalSet` executor (accepts the `!Send` veilid-crypto futures the
>   dioxus runtime accepted), `helpers::retry` (the ported backoff loop),
>   and `tracing` as the one facade.
> - All screens are gpui entities mounted in `RootView`'s phase machine:
>   LoginSignup (guest), PinGate (second-factor), PostRegister (onboarding),
>   ChatList + TOTP + placeholders in the dock.
> - Startup pipeline (`ui::startup::start`): local-init retry →
>   remembered-session admit → network attach, started from the boot
>   closure, setting the hydration latch.
>
> **Verification, all green:** `clippy --features gpui-desktop` 0 errors,
> `clippy --features desktop` (= gpui-desktop now) 0 errors, `clippy
> --features web` / `gpui-web` (wasm) 0 errors, `cargo fmt --check` clean,
> harness `tsc` clean.
>
> **Mobile-pin verification outcome (researched, 2026-11-20):** the pin does
> not compile today, but the gap is small and now scoped:
>
> - The fork's `main` tracks upstream gpui-ce 0.4.0 (only our
>   `core-foundation` relax added), and the core mobile vocabulary is
>   already IN it: `gestures.rs` (touch recognition — `TouchEvent`,
>   `TouchPhase`, `ClickEvent::Touch`), `platform.rs`
>   (`AppLifecyclePhase`, `WindowInsets`, haptics). The web backend
>   already delivers touch events.
> - What's missing is a *platform backend crate* — the fork workspace has
>   no `gpui_ios`/`gpui_mobile` (no UIKit activity loop, no Android
>   `NativeActivity` host).
> - Upstream publishes them as `gpui_ce_ios`/`gpui_ce_mobile` **0.3.0**
>   (published by `philocalyst`, no source repo linked), depending on
>   `gpui-ce ^0.3.0` — one minor behind our fork's 0.4.0 line. The ios
>   crate is small (3,605 LOC, 10 files) and its imports
>   (`AppLifecyclePhase`, `TouchEvent`, `WindowInsets`, `TouchPhase`,
>   `gpui_apple`'s Metal renderer) all exist in fork `main`.
>
> **Path to mobile (chosen):** vendor `gpui_ce_ios` 0.3.0 into the fork
> workspace (`crates/gpui_ios`), bump its `gpui-ce` dep to the fork rev,
> and fix whatever 0.3→0.4 API drift the compiler names (the used symbols
> above all exist, so the drift should be small). Android likewise via
> `gpui_ce_mobile`. Once the fork builds them, the app's mobile pin
> becomes a one-line dependency add plus re-enabling the
> `feature = "mobile"` gates on the platform bridge modules. Until then
> the e2e mobile legs stay on their documented placeholders.

## 6. Cleanup list (execute in Phase 7; nothing earlier — the app must keep building until the cutover)

Delete:
- `Dioxus.toml`, `index.html` (dx placeholders), `tailwind.css`, `overrides.css`, `shared/theme.css`
- `scripts/ci-setup-dx.sh`, `scripts/dx-setup.ts`, `scripts/dx-tailwind-link.sh`
- `.github/workflows/stylesheet.yml`; dx references in the other workflows
- Cargo: `dioxus`, `manganis` deps; `devtools` feature; dx-specific profile tricks (`wasm-release` wasm-opt pass moves to Trunk config)
- `src/platform/window.rs` tao builders, `use_wry_event_handler` glue in `src/app/shell.rs`, the iOS tao event-loop branch in `main.rs`
- `assets/fonts/3270-Regular.woff2` (replaced by `3270-Regular.ttf` in Phase 0); the `@font-face`/preload of the woff2 in `index.html`/`overrides.css` dies with those files
- package.json: `@tailwindcss/*`, `daisyui` devDeps + tailwind scripts; `.npmrc` tailwind link
- `.gitignore` entries for `assets/tailwind.css`, `target/dx*`; add `dist/`

Keep: `build.rs` (capnp/zstd-dict/about — framework-agnostic), `landing/` (independent Astro site), `public/_headers` (extended for COOP/COEP), `native/` + `src/platform/apple_ffi.rs` + `src/platform/android_plugin.rs` (the mobile native hosts are framework-agnostic C-ABI/JNI; they re-attach to the gpui mobile backends once the Phase 0 mobile-backend verification passes), the ios/android rust targets in `rust-toolchain.toml`, the e2e mobile surfaces, all domain modules, `.cargo/config.toml` (build-std/panic-abort stay; wasm flags adjusted to Trunk/threaded needs), Veilid/Automerge/recapn stack.

## 7. Risks

1. **Mobile backend lag** — resolved for iOS (`gpui_ios` vendored into the fork, ported to fork-core 0.4, `ios-sim`/`ios-device` 0 errors, fork rev `8552f16`). Android remains the open lag: no backend exists in the fork or upstream Zed; Android work starts from `platform/android_plugin.rs`'s JNI host once that backend is written.
2. **Web a11y/e2e regression** — canvas rendering drops the DOM contract the harness (and screen readers) rely on; mitigation is seam-marker parity + gpui `TestApp` Rust tests + explicit a11y acceptance in Phase 5.
3. **Toolchain interplay** — nightly `build-std` + `panic=immediate-abort` + gpui's MSRV/panics; threaded wasm needs linker flags + isolation headers; Phase 0 spike exists exactly for this.
4. **Font fidelity** — WOFF2 unsupported; the TTF swap is lossless in glyph data but hinting/rasterization differs per backend (Core Text on Apple, Swash on Linux/web) — verify the terminal aesthetic on each surface in Phase 5.
5. **Visual fidelity** — 563 class strings hand-ported; daisyUI behaviors (avatar, join/btn) rebuilt as widgets; budget a polish pass per screen in Phases 3–4.
6. **Signal→entity correctness** — effects/resources that implicitly tracked signals must be wired to explicit observers; the 13 `GlobalSignal` files and store-revision signals are the highest-risk sites; port them with unit tests around the store trait first. The store-revision design is settled: `impl_crdt_model!` currently stamps a per-model `GlobalSignal<u64>`; the gpui port replaces it with a single `RevisionBus` Global owning an `Entity<RevisionBus>` whose `FxHashMap<TypeId, u64>` counters bump on `notify_model_changed::<T>()`; `use_model_revision::<T>()` becomes an `cx.observe` on that entity filtered by `TypeId` — one observable for all models, cheap per-model identity, no per-model statics.
7. **Parity drift** — `e2e-seams` markers are the contract; freeze them before Phase 3 and diff scenario outcomes at each phase gate.
8. **Bleeding-edge dependency churn** — the fork tracks upstream `main`; a gpui API change can land between the spike and a phase gate. Mitigation: bumps are deliberate (`cargo update -w` + spike rerun) and CI builds all committed targets on every bump — without `--locked` anywhere (repo policy: the lock refreshes, it never gates a build). The counterweight to churn is ownership: the fork is ours, so a breaking upstream change or a gpui bug is patched in the fork the day it bites, not waited out.
9. **Escape hatch — Zed's gpui.** If fork `main` ever blocks a committed target (toolchain, API, or the mobile-backend gap staying open), the fallback is Zed's own `gpui` from [zed-industries/zed](https://github.com/zed-industries/zed/tree/main/crates/gpui) as a git dependency. The port cost is small but real (dependency renames `gpui-ce`→`gpui`, `gpui_ce_platform`→`gpui_platform`, `gpui_ce_elements` has no Zed equivalent — Zed keeps editable text in its own `editor` crate; text raster differs: Zed uses native Core Text/DirectWrite/Swash per platform, gpui-ce uses Parley everywhere; no published standalone crates — git-only, workspace-coupled), so it is a deliberate switch made once, at a phase gate, with the spike re-run — not a hedge maintained in parallel.
