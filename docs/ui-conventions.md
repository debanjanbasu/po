# UI Conventions

**Owns:** `src/components/shared/modal_dialog.rs`

This document centralizes UI implementation rules for Tailwind, DaisyUI, CSS-first interaction patterns, and page sizing.

## Tailwind CSS (Current Best Practices)

Reference: [Tailwind CSS Documentation](https://tailwindcss.com)

Current Tailwind is CSS-first. Prefer configuring in CSS and avoid legacy `tailwind.config.js` unless migrating old code.

### Setup

See `tailwind.css` for the full config. Key plugins:

- `daisyui` (`bumblebee` light + `bumblebee-dark` OLED dark theme with `--prefersdark`)
- `@iconify/tailwind4` (line-md, heroicons)

Source scanning: `@source "./src/**/*.{rs,html,css}"`.

QR image uploads accept PNG, JPEG, and WebP. Keep the file input's MIME list
and the `image` crate decoder features aligned.

The web QR scanner decodes frames on a fixed interval, but skips the decode
work while the document is hidden — a backgrounded tab shows no frames to the
user, so decoding them only drains the battery. The camera stream itself stays
open; only the per-frame decode is skipped.

Theme preference (`"system"` / `"light"` / `"dark"`) is stored in `THEME`, persisted in `AppState`, and synced via DHT.

### Guidance

- Use `@import "tailwindcss";` not legacy `@tailwind base/components/utilities`.
- Keep class names statically detectable — avoid dynamic class-string construction from fragments.
- Use `@source` when classes come from external folders or generated strings.
- Use `@iconify/tailwind4` for icons (`icon-[line-md--alert-circle-twotone-loop]`).
- Use important suffix syntax (`class="font-bold!"`) when needed.
- Style text directly with Tailwind utilities: semantic theme colors such as `text-base-content/60`, explicit hierarchy such as `font-semibold`, and deliberate `text-xs`/`text-sm` sizing where density requires it.

---

## DaisyUI (Current Best Practices)

Reference: [DaisyUI Documentation](https://daisyui.com)

- Configure via CSS `@plugin` blocks, not JS config.
- Switch themes via `data-theme`, not arbitrary class names.
- Prefer semantic tokens (`primary`, `base-*`, `info`) for cross-theme consistency.
- Prefer semantic radius (`rounded-box`, `rounded-field`, `rounded-selector`).
- Avoid `@apply` for DaisyUI modifier classes; prefer component structure and theme tokens.
- Build text hierarchy explicitly with Tailwind font, size, spacing, and semantic color utilities.

---

### Theme tokens and elevation

**The palette is one warm family — no cool hue anywhere.** `shared/theme.css`
defines both themes (`bumblebee` light, `bumblebee-dark`) as a single orange
axis; even the neutrals carry a warm tint, so the surfaces belong to the same
material as the accents. The logo's own palette (warm brown `#4c2a12`, cream
`#fedfab`) anchors it.

| Token | Hue | Role | Light | Dark |
|---|---|---|---|---|
| `primary` | 55 | brand action | `oklch(72% 0.164 55)` | `oklch(76% 0.152 55)` |
| `secondary` | 35 | deeper action | `oklch(56% 0.181 35)` | `oklch(70% 0.178 35)` |
| `accent` | 55 | light surface | `oklch(90% 0.050 55)` | `oklch(92% 0.048 55)` |
| `neutral` | 55 | neutral surface | `oklch(36% 0.030 55)` | `oklch(32% 0.030 55)` |
| `info` | 62 | informational | `oklch(66% 0.140 62)` | `oklch(74% 0.157 62)` |
| `success` | 78 | completion | `oklch(74% 0.142 78)` | `oklch(78% 0.150 78)` |
| `warning` | 92 | caution | `oklch(68% 0.128 92)` | `oklch(80% 0.151 92)` |
| `error` | 25 | failure | `oklch(57% 0.213 25)` | `oklch(66% 0.208 25)` |

`error` at red 25 is the one hue off the orange axis, on purpose: it has to say
"failure" and must not be mistakable for a brand accent.

**Keep every chroma inside sRGB** — 92% of the gamut maximum for that hue at
that lightness. An over-gamut chroma is silently clamped by the browser, and
the result is what makes a warm palette look muddy: the hue survives, the
saturation collapses. That is a real failure mode here, not a theoretical one —
the first pass at this palette clipped and every state turned brown.

**Lightness is the lever between roles, and it is pinned by usage.** In this
app `warning` appears ONLY as `text-warning` (10 sites, no `bg-warning`) and
`primary` is 7 text sites to 1 fill, so both must stay readable *as text on the
page*. Pushing them darker to reach 4.5:1 converges every warm hue on the same
tan (`#d37322` ≈ `#b78321` ≈ `#a78a21`) and the states stop being
distinguishable. So:

* every `-content` on its fill clears **4.8:1** in both themes;
* the dark theme clears **5:1** in both roles for every token;
* in the light theme `secondary` and `error` also clear 4.5:1 as text, while
  `primary`/`success`/`warning`/`info` sit at 2.9–3.7:1 (**large-text grade**,
  the same band daisyUI's own warm themes use — its `sunset` primary is 2.6:1
  on white);
* `base-content` clears 4.5:1 comfortably against both surfaces, which is the
  text people actually read.

**`success` is amber, not green.** That is deliberate: the pairing and sync
completion states are the app's positive moments and they belong to the brand
family. If a state genuinely needs "green = good", add a distinct token rather
than re-hue `success` — the two would stop being distinguishable from the
palette alone.

**Do not list built-in themes in `@plugin "daisyui"`.** `themes: false` is set
on purpose: listing `bumblebee` makes daisyUI apply **its own** token set,
which re-introduces the old yellow at `:where(:root)` — the "system" theme (no
`data-theme` attribute) then falls back to it while the explicit themes look
right. Both palettes come only from our `@plugin "daisyui/theme"` blocks, which
merge our tokens over the built-in ones of the same name. The light theme
carries `default: true` (emitted at `:where(:root)`) and the dark theme
`prefersdark: true` (emitted at `:root:not([data-theme])`), which is what makes
"system" work.

Shape and elevation are theme variables pinned in `shared/theme.css` for both
`bumblebee` and `bumblebee-dark`, so a daisyUI default bump cannot silently
restyle every component:

- `--radius-box: 1rem`, `--radius-field: 0.5rem`, `--radius-selector: 1rem` —
  daisyUI 5.7's bumblebee set.
- `--depth: 1` — daisyUI's inset highlights and component shadows stay on.
- `--border: 1px`, `--noise: 0`, `--size-field`/`--size-selector: 0.25rem`.

Prefer the semantic classes (`rounded-box`, `rounded-field`, `rounded-selector`)
and the semantic colour tokens. Raw Tailwind `rounded-*` utilities bypass the
tokens, so use them only as deliberate exceptions.

`overrides.css` is unlayered, so it beats daisyUI's layers. It carries only what
daisyUI has no token for: `caret-color`, the `:focus-visible` ring, uppercase
tracking on chrome labels (`.dock-label`, `.stat-title`, `.fieldset-legend`,
`.menu-title`), the floating top strip's geometry, and the aura guard described
below. The palette itself lives in `shared/theme.css` only — a second copy is
a second thing to drift.

### Aura

`aura` is a *wrapper*, not a component class: a gradient ring plus a blurred
`::before`/`::after` halo, sized from the child's radius through `:has()`. Use it
on a live action surface, not on always-on chrome:

```rust
div { class: "aura aura-glow self-end",        // send CTA
    button { class: "btn btn-sm btn-secondary", … }
}

div { class: "aura aura-glow w-full",          // TOTP live code card
    div { class: "card bg-base-200 shadow", … }
}
```

`w-full` is required on cards: `.aura` is `display: inline-block`, so an
unwrapped card collapses to content width. Both hosts are live action surfaces —
never put aura on always-visible chrome.

`aura-glow` is a pulsing radial halo with no ring; bare `aura`, `aura-gold`,
`aura-silver`, `aura-dual`, `aura-rainbow` and `aura-holo` draw a rotating ring;
`aura-xs`…`aura-xl` set the ring padding. The gradient paints `currentColor`, so
a coloured wrapper tints the halo and its text. daisyUI only *slows* the
animation fourfold under `prefers-reduced-motion` — treat aura as motion that
stays. `overrides.css` removes the ring and halo when the child is `:disabled`,
because a disabled action must not advertise itself.

### Loading placeholders

Loading lists use daisyUI `skeleton` / `skeleton-text` rows shaped like the real
rows — same padding, avatar size and line heights, `space-y-1` between them — so
nothing shifts when content arrives. Do not put a text colour utility on the same
element as `skeleton-text`: Tailwind utilities are direct members of the
`utilities` layer while daisyUI components live in nested sublayers, so
`text-base-content/50` wins, replaces the component's `background-clip: text` +
`color: transparent`, and paints solid glyphs over the shimmer.

### Silent daisyUI 4 classes

Several daisyUI 4 class names now match nothing in daisyUI 5 and fail silently:
`rounded-btn` (use `rounded-field`), `input-bordered`/`textarea-bordered`
(borders are default) and `placeholder` on `avatar` (use `avatar-placeholder`).
Check a new class against `node_modules/daisyui/components/*.css` first.

## Native CSS & HTML First

Prefer native browser capabilities over Rust-side UI state management. Reserve Dioxus signals for business logic and async state.

### Patterns to prefer

- Hidden checkbox + `:checked` + sibling/descendant selectors for show/hide toggles and animation triggers (e.g., `peer-checked:`, `#id:checked ~ *`).
- HTML `<details>`/`<summary>` for accordions and collapsible sections.
- Radio inputs for mutually-exclusive accordion steps (DaisyUI `collapse` with `name` group).
- CSS `::before`/`::after` with `:has()` for conditional indicators.
- `:valid` / `:invalid` / `:placeholder-shown` with `pattern` / `required` for validation hints.
- CSS `@keyframes` + class toggles for animations.
- Tailwind inline classes in RSX for styling. Use `overrides.css` only when DaisyUI `@layer` specificity blocks utilities.

### When Rust signals are appropriate

- Async operation state (loading, fetch results).
- Business logic derived from storage/network responses.
- Drag-and-drop reorder logic.
- Cross-component shared state via Context API.

### Anti-patterns to avoid

- Per-element boolean signals for visual-only class toggles.
- `format!`-assembled class strings for visual state.
- `use_signal` show/hide toggles where checkbox/`peer-checked`/`<details>` is enough.

---

## Dialogs & Overlays

For true modal UI, prefer the shared `src/components/shared/modal_dialog.rs` shell over ad-hoc fixed overlays.

- Use native `<dialog>` semantics for backdrop handling, Escape close, focus handoff, and focus return.
- Put `autofocus` on the intended starting control inside the modal body; the shared shell handles the surrounding modal behavior.
- Reserve custom fixed-position overlays for non-modal UI (toasts, passive banners, decorative layers) that should not trap focus.
- The shared shell carries `landscape:pl-16`, so a dialog centers on the content
  region beside the landscape dock rail — the same axis page content uses — while
  its backdrop still covers the whole viewport.
- Native iOS share/scanner UI uses `ScenePresentation.topViewController()` from
  the foreground scene's key window. Do not present into an arbitrary connected
  or background scene.

## Reactive Local-First Screens

- Render committed local data; networking runs through the durable queue.
- Mutate collections under the domain's account-scoped RMW API, then invalidate
  their reactive projection. Do not save a detached list captured before an await.
- Reconcile ordered timelines by stable message identity, not vector length.
- Keep input feedback compact and adjacent to the control. Report copy/share
  success only after the platform confirms that outcome; cancellation is not
  an error or a successful copy.
- Local preference hydration precedes authenticated routing. Epoch-scoped gates
  and reactive pending links replace ad-hoc refresh counters or router remounts.
- Recovery polling remains a fallback for missed network events; it is not a
  substitute for reactive committed-state invalidation.

---

## Window chrome (no system borders anywhere)

The app is **distraction-free by default** and looks the same on every
platform: it draws its own chrome and the system's is hidden. On a phone that
means the system bars are gone with the app drawing edge to edge; on a desktop
the window is borderless — no title bar, no frame, no traffic lights — and the
page draws the band that replaces them.

| Platform | What is hidden, and how | Where |
|---|---|---|
| iOS | Status bar: `UIStatusBarHidden` + `UIViewControllerBasedStatusBarAppearance=false`, the app-level pair iOS honours under the scene lifecycle. Home indicator: `prefersHomeIndicatorAutoHidden` on the window tao builds | `native/apple/Info.ios.plist`, `src/platform/window.rs::ios_window` |
| Android | Both system bars (`WindowInsets.Type.systemBars()`), re-applied on every window focus, with `BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE` so they stay reachable by an edge swipe | `native/android/MainActivity.kt::applyDistractionFreeChrome` |
| macOS | Title hidden, title bar transparent, content view full size, traffic lights hidden. The style mask stays, which is what keeps the app menu (⌘Q/⌘W, cut/copy/paste) and the window's own edge resizing | `src/platform/window.rs::desktop_config` |
| Windows / Linux | `decorations(false)`. Windows keeps `WS_SIZEBOX` and tao answers `WM_NCHITTEST`, so the invisible frame still resizes; Linux has nothing left, so the page fills that gap | `src/platform/window.rs::desktop_config` |
| Web | The browser owns its chrome; a tab cannot hide it. Only an installed PWA could, via the manifest's `display` mode — a distribution decision, not a CSS one | — |

### The desktop band belongs to the window

`WindowChrome` (`src/components/shared/window_chrome.rs`) draws what a frame
used to: a strip across the top that moves the window and maximizes it on a
double click, the minimize/maximize/close cluster — the same three buttons in
the same place on every desktop, so the window does not move when the app
crosses platforms — and, on Linux, the resize edges.

Its geometry is `overrides.css`'s (`.window-chrome`,
`.window-caption-cluster`, `.window-resize-edge`), and the shell marks those
builds with `data-window-chrome="desktop"` (`WINDOW_CHROME` in
`src/app/shell.rs`). Four rules hang off that attribute, because the band
belongs to the window rather than to a page: the activity bar, the space pages
reserve for it (`.top-strip-padding`), full-screen pages' own top inset and
the landscape rail all move down by `--window-chrome-height`. Change the band's
height there and they follow.

Two platform differences are real and the code says so where it matters:

- macOS cannot have its window dragged from the page: tao's `drag_window`
  performs the drag with the event AppKit is dispatching *now*, and a page
  press reaches Rust after that. The strip tracks the pointer across an
  overlay and moves the window itself (`DragOverlay`), while Windows and Linux
  hand the drag to the platform (`WM_NCLBUTTONDOWN` and `begin_move_drag`) —
  including Wayland, where tao leaves GTK's client-side decorations on so a
  borderless window can still be moved and resized.
- Windows and macOS still hit-test their own frame edges; Linux does not, so
  the page draws four-pixel edges that ask the window manager for the resize on
  the press itself, the way a compositor expects.

### Phones that change shape

A foldable gets the same decision on its inner and cover displays — the bars
are hidden on both — and what it actually changes is the layout's *size*, so
that is what the app declares: the activity carries `smallestScreenSize` and
`density` among its `configChanges` and is `resizeableActivity`, so unfolding,
switching displays or a multi-window resize reaches the running app instead of
recreating the activity and rebuilding the webview behind it. The layout itself
needs nothing new: it is width- and orientation-driven, and the safe-area
insets it already respects resolve for a foldable's cutout exactly as they do
for a notched phone.

**The safe-area insets still matter** and are not affected by this: they describe
the physical cutout (Dynamic Island / notch) and the home indicator, not the
bars' visibility. Every strip keeps `max(0.5rem, env(safe-area-inset-*))`
(`overrides.css`), which is why hiding the bars gains vertical space instead of
colliding with the housing. The insets only resolve because the shell's viewport
meta carries `viewport-fit=cover` (`src/app/shell.rs`) — see below.

## Floating Top Strip

`ActivityBar` is a fixed, non-modal overlay pinned to the top-right of every
route, so the top strip belongs to it — on desktop the band *above* that strip
belongs to the window's own chrome instead, which is why the bar starts below
`--window-chrome-height` there. The reserved space is owned by the
`.top-strip-padding` rule in `overrides.css`; `NavLayout` applies it to the
page scroll container, which keeps page headers and any other top-right control
clear of the bar without each page inventing its own offset. If the bar's
control row changes height, change the rule, not the pages.

Full-screen `CenteredPage` routes (auth, PIN gate) sit outside `NavLayout` and
keep their own top inset; their content is centered, so the strip stays empty.
Top-anchored toasts (`ToastShell` in `src/components/overlays/error_toast.rs`,
shared by the error and deep-link toasts) reuse the same rule, so a toast
slides in below the bar instead of covering its controls despite the higher
`z-1100`.

### The bar's geometry lives in CSS, not in the component

`overrides.css` is the owner of the strip's geometry (`.activity-bar`,
`.activity-bar-banner`, `.top-strip-padding`). The component carries only the
structural utilities Tailwind expresses well (`fixed z-50 flex items-center
justify-end gap-2 pointer-events-none`).

This is the CSS-first rule in practice: **a project deviation belongs in
`overrides.css`, not in a class string.** The safe-area arithmetic, the
landscape rail offset and the pill's centring are three `env()`/`calc()`
expressions that would otherwise be repeated inline in every component that
needs them — unreadable in Rust, and unlintable by CSS tooling. `overrides.css`
is imported *unlayered* after Tailwind's layers, so its rules win by cascade
order alone; no `!important` anywhere.

### The bar's box is the page's content region

The sync pill that pops up over the strip (the share-side "Syncing your data
to …" banner) is centred **inside the bar**, and the bar is inset to match the
page's content region — in landscape the dock rail takes the left `4rem`
(`landscape:pl-16` on the scroll container), so the bar is inset by the same
`4rem` and its centre matches the content's.

Centring on the raw viewport axis instead left the pill ~40px left of the
page's axis on a desktop window and ~65px off on a phone: the rail shifted the
content right, but the pill did not follow. Anything centred in this app has
to answer the same question — **which axis?** — and the answer is the content
region's, not the viewport's.

**The rail inset is guarded on the rail existing** (`:root:has(.dock)`), not on
the orientation alone. Auth routes (login, pairing, PIN gate) render outside
`NavLayout` and have no rail, so their content centres on the viewport axis; an
unguarded inset pushed the bar 2rem past that axis there. `.rail-inset` is the
same guard for panels — `ModalDialog` uses it, because the activity modal can
be opened from the strip on an auth route.

The bar also respects `env(safe-area-inset-*)` on every edge. A modern iPhone's
front camera (Dynamic Island / notch) eats into the top strip, and in landscape
it eats into a side. The insets only resolve because the shell's viewport meta
carries `viewport-fit=cover` (`src/app/shell.rs`) — without it the CSS
variables are always `0` and the strip sits under the camera housing.

On a narrow screen the pill shares its row with the controls and centres in the
space they leave; a pill centred on the axis *and* clear of the controls would
be capped at ~142px on a 430px bar, which truncates the label to "Sy…". Reading
the label matters more than the last ~65px of centring. On a wide screen there
is room for both: it centres outright (`@media (width >= 40rem)` absolute
positioning) and is capped so it cannot run under the controls.

## Page Text Sizing Convention

Full-page content rendered inside `CenteredPage` should keep a consistent hierarchy and width. Its inner wrapper defaults to `max-w-sm`; pass the `width` prop to select a different Tailwind width class. Use default component and text sizes for primary content, but use `-xs`/`-sm` component variants and `text-xs`/`text-sm` intentionally for dense forms, lists, metadata, and compact controls.

**`CenteredPage` centres its wrapper, not its content.** The wrapper is a block box (`w-full {width}`), so a child narrower than the wrapper is **left-aligned** unless it carries `mx-auto` itself. A `max-w-xs` fieldset inside the default `max-w-sm` wrapper therefore sits 32px left of the page's axis — and the page's logo, which has `mx-auto`, looks centred while the form does not.

Two rules follow:

* A child narrower than the wrapper needs `mx-auto` (a `fieldset w-full max-w-xs mx-auto`).
* A child that should fill a wide pane needs the `width` prop (`width: "max-w-2xl"`), not just a wider class on the child — the wrapper caps the column either way, which leaves symmetric dead bands on a desktop window.

`mx-auto` does **not** centre an inline-block, so DaisyUI's `.loading` spinner needs a
`text-center` (or `flex mx-auto`) parent.

### Standard pattern

```rust
rsx! {
    CenteredPage {
        width: "max-w-md",
        div { class: "w-full",
            h1 { class: "text-base font-bold text-center mb-4", "Page Title" }
            // … page content …
        }
    }
}
```
