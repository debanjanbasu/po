#!/usr/bin/env bash
# Inject the po launcher icons into a dx-generated Android project.
#
# dx 0.8.0-alpha.1 rewrites app/src/main/res with its default ic_launcher on
# every build and implements no [android].icon config, so the only reliable
# override is copying our adaptive-icon drawables over the generated ones and
# re-assembling the APK with the project's own Gradle wrapper. min_sdk is 37,
# so the mipmap-anydpi-v26 adaptive icon covers every supported device.
#
# Usage: android-launcher-icons.sh [android build dir]
#        (default: target/dx/po/debug/android)

set -euo pipefail

base="${1:-target/dx/po/debug/android}"
[ -d "$base" ] || { echo "ERROR: Android build directory not found: $base" >&2; exit 1; }
res="$(find "$base" -type d -path '*/app/src/main/res')"
[[ -n "$res" && "$res" != *$'\n'* ]] || {
  echo "ERROR: expected exactly one app/src/main/res under $base; pass one dx Android build directory." >&2
  exit 1
}

src="$(cd "$(dirname "$0")/.." && pwd)/assets/icons/android"
cp "$src/ic_launcher_foreground.xml" "$res/drawable-v24/ic_launcher_foreground.xml"
cp "$src/ic_launcher_background.xml" "$res/drawable/ic_launcher_background.xml"

root="$(dirname "$(dirname "$(dirname "$(dirname "$res")")")")"
task=assembleDebug
case "$base" in */release/*) task=assembleRelease ;; esac

echo "==> Icons injected; reassembling ($task) in $root"
(cd "$root" && ./gradlew --quiet "$task")
echo "==> Done: APK(s) under $root/app/build/outputs/apk"
