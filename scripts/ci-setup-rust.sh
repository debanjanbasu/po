#!/usr/bin/env bash
# CI-only setup: rust-toolchain.toml is the single source of the rolling channel.

set -euo pipefail

: "${GITHUB_ENV:?Run this setup only inside GitHub Actions}"
: "${GITHUB_STEP_SUMMARY:?GitHub Actions step summary is required}"

cd "$(dirname "$0")/.."
channel="$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust-toolchain.toml)"
[[ "$channel" == "nightly" ]] || {
  echo "ERROR: rust-toolchain.toml must declare the rolling nightly channel" >&2
  exit 1
}

rustup toolchain install "$channel" --profile minimal --no-self-update \
  --component rust-src,rustfmt,clippy "$@"
echo "RUSTUP_TOOLCHAIN=$channel" >> "$GITHUB_ENV"
{
  echo "### Rust: $channel"
  rustup run "$channel" rustc --version --verbose
  rustup run "$channel" cargo --version
} | tee -a "$GITHUB_STEP_SUMMARY"
