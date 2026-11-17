#!/usr/bin/env bash
# Populates ./fonts (gitignored) with the curated font set mink-render loads at boot, and
# regenerates crates/mink-render/src/fa_icons.txt (FontAwesome name -> codepoint table).
# The work happens in tools/fonts/install.sh inside debian:bookworm-slim (the Dockerfile's fonts
# stage runs the same script), and the result is checked against tools/fonts/SHA256SUMS.
#
# Usage (repo root, Docker running, needs network): bash tools/fonts/fetch.sh [out_dir]
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && (pwd -W 2>/dev/null || pwd))"
out="${1:-fonts}"
mkdir -p "$root/$out"

MSYS_NO_PATHCONV=1 docker run --rm -v "$root:/repo" debian:bookworm-slim \
  bash /repo/tools/fonts/install.sh "/repo/$out" /repo/crates/mink-render/src/fa_icons.txt
echo "fonts written to $out"
