#!/usr/bin/env bash
# Normalize dx's generated legacy bundle path (`Po.app`) to the product's lowercase `po.app`.
#
# dx hardcodes `PascalCase(crate_name).app` for iOS/macOS bundle dirs with no
# config override (packages/cli/src/build/request.rs, bundled_app_name), so
# dx therefore emits the legacy filesystem path "Po.app" for the lowercase
# product "po". Renaming a signed .app is safe:
# the code signature does not cover the bundle directory name.
#
# Safe on case-sensitive and case-insensitive filesystems.
#
# Usage: fix-bundle-case.sh <platform-dir>   # e.g. target/dx/po/debug/ios

set -euo pipefail

dir="${1:?usage: fix-bundle-case.sh <platform dir containing dx-generated legacy Po.app>}"
names="$(command ls -1 "$dir")"

if [[ $'\n'"$names"$'\n' == *$'\n'Po.app$'\n'* ]]; then
  [[ $'\n'"$names"$'\n' == *$'\n'po.app$'\n'* ]] && rm -rf "${dir:?}/po.app"
  mv "$dir/Po.app" "$dir/po.app"
elif [[ $'\n'"$names"$'\n' != *$'\n'po.app$'\n'* ]]; then
  echo "ERROR: no po.app (or dx-generated legacy Po.app) bundle in $dir" >&2
  exit 1
fi

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
  cp -f "$(dirname "$0")"/../assets/icons/ios/AppIcon*.png "$app/"
fi
