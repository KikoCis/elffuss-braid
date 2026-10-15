#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 UtopiaIA
#
# Builds the release binary with every build path remapped, then checks that
# the home directory does not appear anywhere inside it. Published binaries are
# built by CI; use this for local release builds.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
# When several prefixes match, rustc applies the last one, so the most general
# goes first.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~ --remap-path-prefix=$rustup_home=/rustup --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$PWD=."

cargo build --release --locked -p elffuss-braid-cli

bin=target/release/braid
if strings "$bin" | grep -qF "$HOME"; then
  echo "build-release: $bin still contains the home directory path" >&2
  exit 1
fi
echo "build-release: $bin contains no home-directory path"
