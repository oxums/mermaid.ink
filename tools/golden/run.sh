#!/usr/bin/env bash
# Rebuild all goldens: encode -> start reference containers -> capture -> pdftoppm -> meta -> stop.
# Needs: docker, host node >= 22, the reference image (default mink-golden). See tools/golden/README.md.
set -euo pipefail
cd "$(dirname "$0")/../.."
IMAGE=${IMAGE:-mink-golden}
OUT=tests/fixtures/golden
FA_URL='https://example.com/ajax/libs/font-awesome/FA_VERSION/css/all.min.css'
export MSYS_NO_PATHCONV=1                  # Git Bash: don't mangle /g, /golden
HOST_PWD=$(pwd -W 2>/dev/null || pwd)       # Git Bash: Windows-style path for bind mounts

stop() { docker rm -f mink-ref-main mink-ref-timeout mink-ref-maxw mink-ref-fa >/dev/null 2>&1 || true; }
trap stop EXIT

node tools/golden/encode.mjs

stop
# Chrome's sandbox needs SYS_ADMIN in Docker (see repo README / AGENTS.md).
start() { local name=$1 port=$2; shift 2; docker run -d --name "$name" --cap-add=SYS_ADMIN -p "$port:3000" "$@" "$IMAGE" >/dev/null; }
start mink-ref-main 3101 -e QUEUE_ADD_TIMEOUT=60000 -e QUEUE_CONCURRENCY=2
start mink-ref-timeout 3102 -e QUEUE_ADD_TIMEOUT=1
start mink-ref-maxw 3103 -e QUEUE_ADD_TIMEOUT=60000 -e MAX_WIDTH=1005
start mink-ref-fa 3104 -e QUEUE_ADD_TIMEOUT=60000 -e FONT_AWESOME_CSS_URL="$FA_URL"

node tools/golden/capture.mjs   # waits for each instance's GET / itself

rm -rf "$OUT/pdfpng" && mkdir -p "$OUT/pdfpng"
docker run --rm -v "$HOST_PWD/$OUT:/g" debian:bookworm-slim sh -ec '
  apt-get update -qq >/dev/null && apt-get install -y -qq poppler-utils >/dev/null
  for f in /g/bodies/*.pdf; do pdftoppm -r 72 -f 1 -l 1 -png -singlefile "$f" "/g/pdfpng/$(basename "$f" .pdf)-p1"; done'

docker run --rm -e IMAGE="$IMAGE" -e IMAGE_ID="$(docker image inspect "$IMAGE" --format '{{.Id}}')" \
  -v "$HOST_PWD/tools/golden:/golden:ro" --entrypoint node "$IMAGE" /golden/meta.mjs > "$OUT/meta.json"

du -sh "$OUT"
