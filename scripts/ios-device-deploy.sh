#!/usr/bin/env bash
# Deploy the dx-built iOS app to a physical device.
#
# Why this script exists — dx 0.8.0-alpha.1 gaps for physical devices:
#   1. dx codesigns only the .app (never the nested DioxusSwiftPlugins
#      .framework) and copies wildcard keychain-access-groups (TEAMID.*)
#      from the provisioning profile, both of which break on device.
#      xtool re-signs the whole bundle correctly: concrete
#      TEAMID.<bundle-id> keychain group, frameworks included, and it
#      provisions/renews the free-team profile automatically.
#   2. dx never passes dev-server coordinates to a physical device, so
#      hot reload cannot connect (simulators inherit env; devices don't).
#      xtool launch cannot inject env vars, so launch goes through
#      xcrun devicectl instead.
#
# One-time setup: node scripts/dx-setup.ts installs xtool, then
# `xtool auth login` (Apple ID; free personal team works). On first use xtool prompts to revoke existing
# Apple Development certificates and mints its own — this invalidates
# Xcode-managed signing on the same team, so let xtool stay the sole signer.
#
# Note: xtool namespaces the on-device app ID as XTL-<team>.<bundle-id>
# (hardcoded upstream). Custom URL schemes still work; universal links only
# match release-signed builds. The launch step resolves the installed ID.
#
# Bundle metadata (name, Bonjour services, permissions) is declarative via
# native/apple/Info.ios.plist + Dioxus.toml and needs no patching here.
#
# Usage:
#   ~/.cargo/bin/dx build --ios --target aarch64-apple-ios
#   ./scripts/ios-device-deploy.sh
#
# All settings are auto-detected; override via .env when needed:
#   APP, BUNDLE_ID, UDID (xtool), DEVICE (devicectl), DEVSERVER_IP, DEVSERVER_PORT
#   SKIP_INSTALL=1  relaunch the already-installed build (hot reload only,
#                   no cable needed - devicectl launches over Wi-Fi)

set -euo pipefail

# .env is optional user config; no need for shellcheck to follow it
[ -f .env ] && { set -a; # shellcheck disable=SC1091
. ./.env; set +a; }

BUILD_DIR="${BUILD_DIR:-target/dx/po/debug/ios}"
BUNDLE_ID="${BUNDLE_ID:-com.debanjanbasu.po}"
if [ -z "${DEVSERVER_IP:-}" ]; then
  DEVSERVER_IP="$(ipconfig getifaddr en0 2>/dev/null || true)"
fi
[ -n "$DEVSERVER_IP" ] || {
  echo "ERROR: en0 has no IPv4 address. Connect Wi-Fi or set DEVSERVER_IP=<IPv4 address>." >&2
  exit 1
}
DEVSERVER_PORT="${DEVSERVER_PORT:-8080}"

command -v xtool >/dev/null 2>&1 || {
  echo "ERROR: xtool not found. Run 'node scripts/dx-setup.ts' (brew install xtool-org/tap/xtool), then 'xtool auth login'" >&2
  exit 1
}

if [ -n "${SKIP_INSTALL:-}" ]; then
  echo "==> SKIP_INSTALL=1 - skipping install, relaunching what's on the phone"
else
  if [ -z "${APP:-}" ]; then
    "$(dirname "$0")/fix-bundle-case.sh" "$BUILD_DIR"
    APP="$BUILD_DIR/po.app"
  fi
  [ -d "$APP" ] || { echo "ERROR: $APP not found - run the dx build first." >&2; exit 1; }

  # dx 0.8.0-alpha.1 has no iOS icon pipeline, so copy the launcher PNGs
  # declared under CFBundleIcons in native/apple/Info.ios.plist into the bundle root.
  # Runs before xtool, which signs the whole bundle afterwards.
  echo "==> Injecting launcher icons"
  cp "$(dirname "$0")/../assets/icons/ios/"AppIcon*.png "$APP/"

  echo "==> Signing + installing via xtool"
  # xtool discovers devices via usbmuxd (USB cable or Finder Wi-Fi-sync
  # entries) - NOT CoreDevice, which is what lets devicectl reach the phone
  # over plain Wi-Fi. So `xtool install` can hang "Waiting for device..."
  # while devicectl sees the phone fine. Worse, cancelling xtool with ^C
  # exits 0, so set -e doesn't stop the script and it relaunches the STALE
  # build already on the phone. Probe first with --no-wait and fail fast.
  # A single transport is dialed (USB preferred): xtool's default --all mode
  # dials stale Wi-Fi usbmux entries, failing with LockdownClient muxError
  # before it ever tries USB.
  MODE=""
  prompted=""
  for _ in $(seq 1 10); do
    if [ -n "$(xtool devices --usb --no-wait 2>/dev/null)" ]; then
      MODE="--usb"; break
    elif [ -n "$(xtool devices --network --no-wait 2>/dev/null)" ]; then
      MODE="--network"; break
    fi
    if [ -z "$prompted" ]; then
      echo "    no device visible to xtool - plug in the USB cable (waiting up to 30s)..."
      prompted=1
    fi
    sleep 3
  done
  if [ -z "$MODE" ]; then
    echo "ERROR: xtool can't see the iPhone (usbmuxd has no USB or Wi-Fi-sync entry)." >&2
    echo "  - Plug in the cable and unlock the phone, then re-run." >&2
    echo "  - For cable-free installs, enable Finder > iPhone > 'Show this iPhone when on Wi-Fi'." >&2
    echo "  - Or re-run with SKIP_INSTALL=1 to just relaunch the installed build." >&2
    exit 1
  fi
  echo "    installing over ${MODE#--}"
  xtool install "$MODE" ${UDID:+--udid "$UDID"} "$APP" || {
    echo "ERROR: xtool install over ${MODE#--} failed. If this is muxError on a stale" >&2
    echo "Wi-Fi entry, plug in the USB cable and re-run." >&2
    exit 1
  }
fi

echo "==> Locating device for launch"
if [ -z "${DEVICE:-}" ]; then
  DEVICE=$(xcrun devicectl list devices 2>/dev/null | grep -E '\b(connected|available)\b' |
    grep -Eo '[0-9A-F]{8}(-[0-9A-F]{4}){3}-[0-9A-F]{12}' | head -1)
fi
[ -n "$DEVICE" ] || { echo "ERROR: no connected device - plug it in or set DEVICE=<CoreDevice UUID>." >&2; exit 1; }
echo "    $DEVICE"

echo "==> Resolving installed bundle ID"
# Querying apps needs the developer disk image mounted, which a locked phone
# refuses (kAMDMobileImageMounterDeviceLocked) - poll while the user unlocks.
# `|| true` keeps set -euo pipefail from killing the script on a failed query.
LAUNCH_ID=""
prompted=""
for _ in $(seq 1 20); do
  LAUNCH_ID=$(xcrun devicectl device info apps --device "$DEVICE" 2>/dev/null |
    grep -Eo "[A-Za-z0-9.-]*${BUNDLE_ID}" | head -1 || true)
  [ -n "$LAUNCH_ID" ] && break
  if [ -z "$prompted" ]; then
    echo "    can't query apps yet - if the iPhone is locked, unlock it (waiting up to 60s)..."
    prompted=1
  fi
  sleep 3
done
LAUNCH_ID="${LAUNCH_ID:-$BUNDLE_ID}"
echo "    $LAUNCH_ID"

echo "==> Launching with dev server at ${DEVSERVER_IP}:${DEVSERVER_PORT}"
launch() {
  xcrun devicectl device process launch --terminate-existing \
    --environment-variables "{\"DIOXUS_DEVSERVER_IP\":\"${DEVSERVER_IP}\",\"DIOXUS_DEVSERVER_PORT\":\"${DEVSERVER_PORT}\",\"DIOXUS_CLI_ENABLED\":\"true\"}" \
    --device "$DEVICE" "$LAUNCH_ID" 2>&1
}

# The app is installed by this point, so launch failures are almost always a
# locked screen (poll while the user unlocks) or transient tunnel churn (one
# quick retry). Anything else fails fast with devicectl's output.
retried=""
prompted=""
for _ in $(seq 1 20); do
  if out=$(launch); then
    echo "$out"
    echo "==> Done"
    exit 0
  fi
  if grep -q 'BSErrorCodeDescription = Locked' <<<"$out"; then
    if [ -z "$prompted" ]; then
      echo "    iPhone is locked - unlock it to launch (waiting up to 60s)..."
      prompted=1
    fi
    sleep 3
  elif [ -z "$retried" ]; then
    echo "    launch failed (tunnel churn?) - retrying once"
    retried=1
    sleep 3
  else
    echo "$out" >&2
    exit 1
  fi
done
echo "ERROR: device stayed locked. Unlock it and re-run - the app is already installed." >&2
exit 1
