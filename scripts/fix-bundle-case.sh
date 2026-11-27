#!/usr/bin/env bash
# Inject the app icons into a dx-built Apple bundle.
#
# `[application] name = "po"` in Dioxus.toml already makes dx emit the
# lowercase `po.app` bundle dir directly (no rename needed), but `dx build`
# still has no icon pipeline — `[bundle].icon` only affects `dx bundle` — so
# the icons the plists declare are copied in post-build.
#
# Safe on case-sensitive and case-insensitive filesystems.
#
# Usage: fix-bundle-case.sh <platform-dir>   # e.g. target/dx/po/debug/ios

set -euo pipefail

dir="${1:?usage: fix-bundle-case.sh <platform dir containing the dx-built po.app>}"

# macOS bundles only: dx build never copies the icon that native/apple/Info.macos.plist
# declares (CFBundleIconFile = icon.icns), so inject it post-build. Copy
# unconditionally so regenerated icons replace stale ones.
res="$dir/po.app/Contents/Resources"
if [ -d "$res" ]; then
  cp -f "$(dirname "$0")/../assets/icons/icon.icns" "$res/icon.icns"
fi

# iOS bundles are flat — no Contents/ — and the same dx gap leaves the launcher
# PNGs that native/apple/Info.ios.plist's CFBundleIcons declares uncopied.
# Copy unconditionally so regenerated icons replace stale ones.
app="$dir/po.app"
if [ ! -d "$app/Contents" ]; then
  if [ ! -d "$app" ]; then
    echo "ERROR: no po.app bundle in $dir" >&2
    exit 1
  fi
  cp -f "$(dirname "$0")"/../assets/icons/ios/AppIcon*.png "$app/"
fi
