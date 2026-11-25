#!/usr/bin/env bash
# CI-only: put the Dioxus CLI that `Cargo.lock` pins on PATH for later steps.
#
# One script for the three tandem jobs (web, apple, android) rather than the
# same fifteen lines in each. A correction to one copy and not the others stays
# invisible until each job fails in turn — which is what happened when this
# install was first added and the job then called `dx` bare in the same step.
#
# Env: GITHUB_PATH (appended, so LATER steps get `dx` from PATH), and
# CARGO_TARGET_DIR, which the calling job sets to keep this host build out of
# the app's target directory.

set -euo pipefail

: "${GITHUB_PATH:?Run this setup only inside GitHub Actions}"

root="${DX_INSTALL_ROOT:-$HOME/.ci-bin}"
dx_bin="$root/bin/dx"

# `$GITHUB_PATH` only affects later steps, so every invocation below resolves
# the binary by path — a bare `dx` here is "command not found" even after a
# successful install.
echo "$root/bin" >> "$GITHUB_PATH"

revision="$(sed -n 's|^source = "git+https://github.com/debanjanbasu/dioxus\.git?branch=main#\([0-9a-f]\{40\}\)"$|\1|p' Cargo.lock | sort -u)"
if [[ ! "$revision" =~ ^[0-9a-f]{40}$ ]]; then
  echo "ERROR: expected one shared locked revision for Dioxus and Manganis" >&2
  exit 1
fi

# The cache key folds in Cargo.lock, so a restored CLI should already be this
# revision; assert it instead of accumulating silent staleness.
reusable=false
if [[ -x "$dx_bin" ]]; then
  version="$("$dx_bin" --version)"
  if [[ "$version" == *"${revision:0:7}"* ]]; then
    reusable=true
    echo "Reusing $version"
  else
    echo "Cached dx is not the locked revision ($version); reinstalling" >&2
  fi
fi

if [[ "$reusable" == false ]]; then
  cargo install --locked --root "$root" \
    --git https://github.com/debanjanbasu/dioxus.git --rev "$revision" dioxus-cli
  version="$("$dx_bin" --version)"
fi

if [[ "$version" != *"${revision:0:7}"* ]]; then
  echo "ERROR: dx does not report the locked revision ${revision:0:7}: $version" >&2
  exit 1
fi
echo "$version" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
