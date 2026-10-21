#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 UtopiaIA
#
# Rebuilds the README's images in docs/assets: the animated logo, the
# diagram, the terminal recordings and the banner. The recordings run the
# real CLI, built from this checkout, in a throwaway directory; agg
# (https://github.com/asciinema/agg) turns them into GIFs, and the banner
# shows the landings of the same agents recording, ids and all.
# Needs cargo, python3, git, jq and agg.
set -euo pipefail
cd "$(dirname "$0")/../.."

cargo build --release --locked -p elffuss-braid-cli
braid="${CARGO_TARGET_DIR:-target}/release/braid"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

python3 scripts/demos/logo.py site/index.html docs/assets/braid-animated.svg
python3 scripts/demos/diagram.py site/index.html site/site.css docs/assets
python3 scripts/demos/record.py "$braid" "$work/run" "$work/casts"
python3 scripts/demos/hero.py site/index.html "$work/casts/agents.cast" docs/assets

# The site's terminal palette: background, foreground, then the 16 ANSI colors.
theme=0e0f18,eae8f2,1a1b29,e779cf,54dcc6,c8a06a,8272ff,e779cf,54dcc6,eae8f2,7f7ca0,f0a0e0,7fe8d6,dcc08e,a497ff,f0a0e0,7fe8d6,ffffff
for cast in "$work"/casts/*.cast; do
  name="$(basename "$cast" .cast)"
  agg -q --theme "$theme" --font-size 15 --fps-cap 20 --idle-time-limit 4 \
    --last-frame-duration 4 "$cast" "docs/assets/demo-$name.gif"
  echo "docs/assets/demo-$name.gif"
done
