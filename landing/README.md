# oo Landing Site

Static landing page for **[oo-app.pages.dev](https://oo-app.pages.dev)** — handles contact invites, device pairing redirects, and marketing.

Built with [Astro](https://astro.build) + [Tailwind CSS v4](https://tailwindcss.com) + [DaisyUI 5](https://daisyui.com) (bumblebee theme) to match the app's look and feel.

## Quick Start

The root `.nvmrc` contains nvm's `node` alias. Run `nvm install` to fetch the
current latest locally; Cloudflare Pages reads the same file. npm dependencies
are managed by the root manifest and lockfile.

```bash
npm ci
npm run landing:dev        # http://localhost:4321
```

## Commands

| Command | Action |
|---------|--------|
| `npm run landing:dev` | Start dev server at `localhost:4321` with HMR |
| `npm run landing:build` | Build static site to `landing/dist/` |
| `npm run landing:preview` | Preview production build locally |

## Deploy

Hosted on [Cloudflare Pages](https://pages.cloudflare.com) (free tier) with
**Git integration**. If custom build-watch paths are configured, include
`landing/**` plus the root `package.json`, `package-lock.json`, and `.nvmrc`.

### Setup (one-time)

1. Go to [Cloudflare Dashboard](https://dash.cloudflare.com) → Pages → Create a project → Connect to Git
2. Select the `debanjanbasu/oo` repository
3. Configure build settings:
   - **Root directory**: repository root (leave blank/default)
   - **Build command**: `npm run landing:build`
   - **Build output directory**: `landing/dist`
   - **Node version**: leave `NODE_VERSION` unset; Pages reads root `.nvmrc`
     (`node` alias) to select the latest release.
4. Save and deploy

Preview deployments are created automatically for pull requests.

Production URL: **https://oo-app.pages.dev**

## Pages

| Route | Purpose |
|-------|---------|
| `/` | Marketing home + client-side invite/pairing handler |
| `/c/{username}/{dht_key}` | Canonical contact invite — shows "Connect with @user", opens the app, copyable identity |
| `/p/{payload}` | Canonical device pairing link — opens the app with `oo://pair?data=...` fallback |
| `/privacy/` | Privacy policy |
| `/terms/` | Terms of service |

## Security & Sync Features

oo uses HPKE RFC 9180 for per-member conversation key wrapping and transactional DHT sync for atomic multi-record writes. Automerge rescue is used on hydration to recover from partial documents. These are marketed as “HPKE-secured chat” and “transactional DHT sync” on the landing home.

Canonical share URLs use path-based universal/app-link routes (`/c/*`, `/p/*`). `public/_redirects` rewrites those paths to `/` with a `200` response so the landing fallback can preserve the original browser URL while `src/lib/share-route.ts` owns the route/deep-link parsing/building logic used by `src/pages/index.astro`.

## Theme

Matches the app's design system:

- **Font**: IBM3270 monospace (self-hosted woff2 at `public/fonts/`)
- **Theme**: DaisyUI 5 bumblebee on light preference and the app-matched bumblebee-dark palette for system dark preference.
- **Icons**: `@iconify/tailwind4` with `line-md` and `heroicons` prefixes
- **CSS**: Tailwind v4 CSS-first config via `@tailwindcss/vite`

Theme tokens and Iconify configuration live in `../shared/theme.css`, imported by both the app and landing stylesheet; each keeps its own source globs, supplemental plugins, and font-face rules.

## Structure

```
landing/
├── public/
│   ├── fonts/3270-Regular.woff2       # IBM3270 font
│   ├── .well-known/
│   │   ├── apple-app-site-association # iOS Universal Links
│   │   └── assetlinks.json            # Android App Links
│   ├── _headers                       # Cloudflare Pages headers
│   └── _redirects                     # `/c/*` + `/p/*` rewrites to the root page
├── src/
│   ├── layouts/Base.astro             # HTML shell with theme setup
│   ├── lib/share-route.ts             # Shared `/c/*` + `/p/*` parsing/building helpers
│   ├── pages/
│   │   ├── index.astro                # Render-focused home + invite/pairing fallback UI
│   │   ├── privacy/index.astro        # Privacy policy
│   │   └── terms/index.astro          # Terms of service
│   └── styles/global.css              # Tailwind + DaisyUI + font config
├── astro.config.ts                   # Astro config with Tailwind vite plugin
├── tsconfig.json
└── .gitignore
```

## App Integration

The Dioxus app generates share URLs pointing here:

- **Contact**: `https://oo-app.pages.dev/c/{username}/{dht_key}` via `share_urls::contact_share_url()`
- **Pairing**: `https://oo-app.pages.dev/p/{payload}` via `share_urls::pairing_share_url()`
- **Compact identity**: `oo:{username}#{dht_key}` via `share_urls::compact_identity()` (offline sharing)

The `CONTACT_SHARE_URL_PREFIX` and `PAIRING_SHARE_URL_PREFIX` constants live in `../src/platform/share_urls.rs`.

## Deep Links

The landing page attempts to open the native app via `oo://` scheme after a path-based share URL is opened in the browser. Route parsing plus deep-link/identity builders live in `src/lib/share-route.ts` so landing route spec changes stay in one place:

- `oo://chat?data=<base64url(username|dht_key)>` — contact invite
- `oo://pair?data={payload}` — device pairing

If the app isn't installed, the page shows accurate "not publicly available yet" messaging plus a real GitHub project link instead of disabled store buttons.

## Release Blockers: Universal Links / App Links

App association files in `public/.well-known/` are intentionally blocked for release in this workspace because the production signing identifiers are not checked in.

- `apple-app-site-association` currently uses `RELEASE_BLOCKER_APPLE_TEAM_ID.com.debanjanbasu.oo`. Replace `RELEASE_BLOCKER_APPLE_TEAM_ID` with the real 10-character Apple Team ID for bundle identifier `com.debanjanbasu.oo` before shipping.
- `assetlinks.json` currently uses `RELEASE_BLOCKER_ANDROID_SHA256_CERT_FINGERPRINT`. Replace it with the real colon-delimited SHA-256 fingerprint(s) for the production Android signing certificate(s) used to ship package `com.debanjanbasu.oo` (for Google Play, use the Play App Signing certificate fingerprint).
- Keep the association path rules on `/c/*` and `/p/*`.
