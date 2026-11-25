#!/usr/bin/env bash
# Point dx's Tailwind slot at the npm-installed Tailwind CLI.
#
# dx auto-runs Tailwind during `dx build`/`dx serve`, downloading a standalone
# binary from GitHub releases into its managed tools directory — but it
# swallows install/run failures (packages/cli/src/build/request.rs pipes
# stderr to null and ignores the exit code). On networks that block GitHub
# release downloads the install fails on every build and dx silently ships a
# stale assets/tailwind.css. The tell: "Installing tailwindcss@vX" printed on
# every build.
#
# The expected <TAG> is hardcoded in dx (packages/cli/src/tailwind.rs, V4_TAG),
# so it moves when dx is updated. This script asks the *installed* dx where it
# expects the binary (`dx doctor` prints the path unconditionally) and links
# that slot to node_modules/.bin/tailwindcss (version pinned in package-lock.json).
#
# Run once after every dx install/update:
#   npm ci
#   ./scripts/dx-tailwind-link.sh
# DX_BIN selects an explicit Dioxus CLI. Windows additionally requires
# TAILWIND_BIN pointing to the matching standalone .exe (npm's shim is not an .exe).
#
# If a future dx needs a newer Tailwind feature, bump @tailwindcss/cli in
# package.json — dx only uses the tag to name the install dir; any v4 CLI
# speaks the same --input/--output/--watch interface.

set -euo pipefail

cd "$(dirname "$0")/.."

dx_bin="${DX_BIN:-$HOME/.cargo/bin/dx}"
if [ ! -x "$dx_bin" ]; then
  if [ -n "${DX_BIN:-}" ] || ! dx_bin="$(command -v dx)"; then
    echo "error: Dioxus CLI not found; set DX_BIN to the installed Dioxus executable" >&2
    exit 1
  fi
fi
dx_version="$("$dx_bin" --version)"
case "$dx_version" in
  dioxus\ *) ;;
  *)
    echo "error: $dx_bin is not the Dioxus CLI; set DX_BIN (PATH may contain Deno's dx)" >&2
    exit 1
    ;;
esac

tailwind_bin="${TAILWIND_BIN:-$PWD/node_modules/.bin/tailwindcss}"
[ -x "$tailwind_bin" ] || {
  echo "error: Tailwind CLI not found at $tailwind_bin; restore locked dependencies with npm ci" >&2
  exit 1
}

esc="$(printf '\033')"
expected="$("$dx_bin" doctor \
  | tr -d '\r' \
  | sed "s/${esc}\[[0-9;]*m//g" \
  | sed -n 's/^[[:space:]]*TailwindCSS:[[:space:]]*//p')"

if [[ -z "$expected" || "$expected" == *$'\n'* ]]; then
  echo "error: could not read expected Tailwind path from 'dx doctor'" >&2
  exit 1
fi

dx_home="${DX_HOME:-$HOME/.dx}"
case "$OSTYPE" in
  linux*)
    dx_home="${DX_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/.dx}"
    ;;
  msys*|cygwin*)
    tailwind_bin="$(cygpath -u "$tailwind_bin")"
    expected="$(cygpath -u "$expected")"
    dx_home="$(cygpath -u "${DX_HOME:-${USERPROFILE:-$HOME}/.dx}")"
    case "$tailwind_bin" in
      *.exe) ;;
      *)
        echo "error: Windows requires TAILWIND_BIN to name a native standalone .exe" >&2
        exit 1
        ;;
    esac
    ;;
esac

case "$tailwind_bin" in
  /*) ;;
  *) tailwind_bin="$PWD/$tailwind_bin" ;;
esac

case "$expected" in
  "$dx_home/tools/"*) ;;
  *)
    # no_downloads selects an existing PATH tool; never overwrite it.
    "$expected" --help >/dev/null
    echo "dx resolves Tailwind outside its managed tools directory ($expected); leaving it unchanged" >&2
    exit 0
    ;;
esac

mkdir -p "$(dirname "$expected")"
case "$OSTYPE" in
  msys*|cygwin*)
    if [ -L "$expected" ]; then rm "$expected"; fi
    cp "$tailwind_bin" "$expected"
    ;;
  *) ln -sf "$tailwind_bin" "$expected" ;;
esac

"$expected" --help >/dev/null || {
  echo "error: linked binary at $expected does not run" >&2
  exit 1
}

echo "configured $expected -> $tailwind_bin"
