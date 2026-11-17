# mermaid.ink Rust fork — approved plan

Hard fork. Everything is rewritten in Rust. Chrome/Puppeteer/Node/Postgres are removed entirely (no fallback).
The behaviour reference is upstream jihchi/mermaid.ink at commit 588671e with `mermaid` bumped to 12.3.0 (the Node/Puppeteer service). It is not in this repo: the parity fixtures in `tests/fixtures/` were recorded from it, and `tools/golden/README.md` explains how to re-record them. Paths like `src/helpers/*.js` below refer to that upstream tree.

## Stack (pinned)

The direct dependencies actually used (see the `Cargo.toml` files; versions are the minimums, Cargo.lock pins the rest).

| Purpose | Crate | Used by |
|---|---|---|
| Renderer | `merman = "=0.10.0"` (features png, jpeg, pdf, math, native-fonts, system-clock; re-exports merman-export 0.10.0), `merman-render = "=0.10.0"` (no default features, for `RenderEnvironment`) | mink-render |
| SVG/PDF helpers | roxmltree 0.21, svgtypes 0.16, png 0.18, lopdf 0.47 (no default features), base64 0.23 | mink-render |
| WebP | libwebp-sys 0.14.4 (`WebPEncodeRGBA`, q=90) | mink-render |
| HTTP | axum 0.8.9, hyper 1.12 (server, http1), hyper-util 0.1.21 (graceful shutdown, header read timeout), tokio 1.54, bytes 1 | server |
| TLS | rustls 0.23 with the `aws_lc_rs` provider only | server, mink-cache |
| Memory cache | moka 0.12.16 `future`, `EvictionPolicy::lru()`, byte weigher | mink-cache, mink-fetch |
| Optional L2 | redis 1.8 (tokio-comp, connection-manager, tokio-rustls-comp, tls-rustls-webpki-roots) | mink-cache |
| Remote images | reqwest 0.13.5 (rustls only, no gzip/brotli, `retry(never)`), hickory-resolver 0.26, imagesize 0.15 | mink-fetch |
| Decode | flate2 1.1 (zlib + gzip, output capped with `take`), base64 0.23 (lenient, js-base64 semantics), serde_json 1, ryu-js 1.0 (JS number formatting) | mink-core |
| Hash | sha2 0.11 | mink-core, server |
| Logging | tracing 0.1 + tracing-subscriber 0.3 (env-filter) | server |
| Allocator | mimalloc 0.1 | server |
| Test-only | image 0.25, image-compare 0.5, hayro 0.9 (PDF raster), lopdf 0.47, roxmltree 0.21, proptest 1, rcgen 0.14, tokio-rustls 0.26, toml 0.9, tower 0.5, http-body-util 0.1, serde 1 | tests |

tower-http is not used directly (CORS and errors are hand-written to match upstream byte for byte).

## Workspace layout

```
Cargo.toml          # workspace root and the server package `mermaid-ink` (src/: config, routes, admission)
crates/mink-core    # decode, query parsing (JS semantics), cache keys
crates/mink-render  # merman rendering, fonts, FA glyphs, svg post-processing, raster, pdf
crates/mink-fetch   # hardened https image fetcher
crates/mink-cache   # moka LRU + optional Redis L2
static/index.html   # home page, embedded at compile time
tests/              # server-level tests + fixtures (vectors, golden, corpus, divergences.toml)
tools/fonts         # fetch.sh / install.sh: the pinned font set, checked against SHA256SUMS
tools/golden        # one-time capture scripts against the upstream reference image (not run in CI)
Dockerfile          # fonts stage (tools/fonts/install.sh) + cargo build + distroless/cc-debian12:nonroot
```

Do not add direct usvg/resvg dependencies at versions other than merman's (0.47).

## Rendering pipeline

1. Decode `:encodedCode`: a literal port of `src/helpers/getOptionsFromCode.js`. The inflate output is capped at `MAX_DECODED_BYTES`, and the URL at `MAX_URL_BYTES` (431).
2. Config:
   - The URL `mermaid` JSON (string → JSON.parse, otherwise used as-is) becomes the merman site config.
   - Force `securityLevel: loose`, apply the `?theme` override, and use Mermaid's six-key secure list (strip any user `secure`).
   - Enable the RawThemeCss trusted lane.
   - If the code is longer than `maxTextSize` (default 50000, UTF-16 length), substitute Mermaid's `graph TB;a[Maximum text size in diagram exceeded];style a fill:#faa` and return 200.
3. Layout:
   - Use native font measurement with the same `NativeFontContext` used for painting. Fonts come from `FONT_DIR`, are loaded once into memory, and Liberation Sans is first.
   - If merman rejects native measurement (themeCSS or inline font-family), fall back to deterministic measurement.
   - Runtime policy: `RuntimePolicy::deterministic().try_with_system_clock()`, so the gantt "today" line is real.
   - The layout work-unit limit is overridden from `MAX_LAYOUT_WORK_UNITS`. Each request gets an `OperationControl` deadline.
4. Remote images (`REMOTE_IMAGES`):
   - Prefetch the https `<image href>` / `img:` URLs for every endpoint and feed their natural sizes into layout.
   - /svg keeps the original href. Raster and PDF output inline the image as a `data:` URL.
   - Any failed fetch marks the render as degraded, and degraded renders are not cached.
5. /svg post-processing:
   - Same as today: xmlns:xlink, FontAwesome `<style>@import url(...)</style>`, bgColor, and setSize (width/height and max-width).
   - New: strip every `on*` attribute and every `<script>` element, and add a `Content-Security-Policy` header.
6. Raster:
   - `fa:` `<i class>` icons become FA7 font glyphs.
   - Then `finalize_resvg_svg` → `svg_to_png/jpeg_controlled`.
   - Default fit width is 1904 px (Chrome's 1920 viewport minus the 8 px body margin).
   - JPEG is quality 90 over white or bgColor.
   - WebP is PNG → RGBA → libwebp at q90. Over 16383 px returns 400.
7. PDF:
   - `fit` → FitSvg.
   - `paper`/`landscape` → page sizes. Tall diagrams paginate.
8. Renders run on `spawn_blocking`. Admission:
   - A semaphore with `QUEUE_CONCURRENCY` permits.
   - A weighted memory semaphore with `RENDER_MEMORY_BUDGET_MB`.
   - Once `QUEUE_MAX_PENDING` requests are waiting, further requests get 503 immediately.
   - A panic becomes 500. Never set `panic = "abort"`.

## Remote image fetcher

- **Allowed requests:** https only, port 443. Proxies are off (`no_proxy`). Redirects are limited to 3, and every hop is re-validated.
- **DNS and IP policy:**
  - DNS goes through hickory with a 1 s timeout. Every resolved address must be public, and the connection goes to exactly the validated addresses.
  - IP literals are checked against the same policy.
  - Denied: unspecified, loopback, private, link-local (including 169.254.169.254), broadcast, documentation, multicast, CGNAT 100.64/10, 192.0.0/24, 198.18/15, 240/4, 0/8, fc00::/7, fe80::/10, fec0::/10. Also denied: IPv4-mapped, NAT64 (64:ff9b::/96, 64:ff9b:1::/48), 6to4 and Teredo forms that wrap a denied v4 address.
  - IPv6 is allowed only within 2000::/3.
- **Timeouts:** connect 1 s, read 1 s, total `REMOTE_IMAGE_TIMEOUT_MS` per image, and a deadline for the whole fetch phase.
- **Content:** at most `REMOTE_IMAGE_MAX_BYTES` per image, enforced while streaming. Content-type and magic bytes must both be png, jpeg, gif or webp. SVG is refused.
- **Limits:**
  - At most `REMOTE_IMAGE_MAX_COUNT` images per diagram.
  - 4 concurrent fetches per host and 32 globally.
  - The moka url → result cache keeps successes and failures for 60 s.
- **Policy injection:** the IP policy is a constructor argument. Only `#[cfg(test)]` code may build a loopback-allowing fetcher, never an env var.

## Caching

- **Key:** `config_fingerprint` (merman version, font set identity, FA URL, remote images on/off, layout work units) + the old `createCacheKey` hex.
- **L1:** moka with LRU eviction and a byte weigher. Size is `CACHE_MEMORY_MB` (default 25; 0 disables). It stores `{status, mime, body}`: 200 and 400 are cached, 5xx and degraded renders are not. `get_with` coalesces concurrent misses for the same key.
- **L2 (optional):** `REDIS_URL`, key `mink:v1:<key>`, `SET EX REDIS_TTL_SECS`.
  - Boot never waits on Redis. GETs time out after `REDIS_TIMEOUT_MS`. SETs are fire-and-forget.
  - Any Redis error is treated as a miss.
- **HTTP:** on 200 and 400 responses, `Cache-Control: public, max-age=HTTP_CACHE_MAX_AGE` (0 means no header) plus an ETag.

## Compatibility contract

The behaviour contract comes from the upstream Node code; the parity fixtures are the arbiter:
- Routes are case-insensitive, accept an optional trailing slash, and HEAD is handled as GET.
- CORS mirrors the Origin (or `*`) with `Vary: Origin`. Preflight returns 204 on any path.
- Error bodies are text/plain. 5xx bodies are the status text.

Our own validation messages stay byte-for-byte identical, as do `Unknown serde type: x` and `Unknown diagram error`. Mermaid parse errors are 400 with merman's message.

Intentional divergences, listed in `tests/fixtures/divergences.toml`:
- `Retry-After: 1` on 503.
- Repeated query params use the first value (today they cause a 500).
- CSP on /svg, with `on*` attributes and `<script>` stripped.
- `QUEUE_CONCURRENCY` defaults to the CPU count.
- Decode and URL caps.
- Merman's parse error text.
- Merman-only diagram types.

## Tests

1. Port every JS test 1:1: `tests/query.rs`, `tests/cache_key.rs`, `tests/kitchensink.rs`. The Redis tests replace `db.test.js`.
2. Vectors (`tests/fixtures/vectors/*.json`), generated by `tools/golden/gen-vectors.mjs` inside the reference image from the real JS helpers. Rust must match them exactly. Proptest checks invariants.
3. Goldens (`tests/fixtures/golden/`), captured by `tools/golden/capture.mjs` from the reference image. The corpus is the K, T, F and D series plus the parameter matrix (see `tools/golden/README.md`).
   - **Exact:** status, mime, CORS headers, our own error text, explicitly requested dimensions, label set, node/edge/cluster counts.
   - **±4%:** derived dimensions.
   - **SSIM:** at least 0.90 for raster, at least 0.85 for the first PDF page.
4. `tests/hardening.rs`:
   - XSS payloads, inflate bomb, oversized URI.
   - Load shedding, deadline → 503, panic → 500.
   - WebP over 16383 px, nesting depth.
   - SSRF IP table, redirect to loopback.
   - Redis down, LRU byte bound.
   - Determinism (render twice, identical bytes), graceful shutdown.

## Environment variables

| Var | Default | Notes |
|---|---|---|
| `HOST` | `0.0.0.0` | |
| `PORT` | `3000` | |
| `MAX_CONNECTIONS` | `10000` | Open client connections |
| `MAX_WIDTH` | `10000` | Parsed with JS semantics |
| `MAX_HEIGHT` | `10000` | Parsed with JS semantics |
| `FONT_AWESOME_CSS_URL` | cdnjs | `FA_VERSION` is substituted |
| `QUEUE_CONCURRENCY` | CPU count | |
| `QUEUE_ADD_TIMEOUT` | `3000` | Render deadline in ms |
| `QUEUE_MAX_PENDING` | `256` | |
| `RENDER_MEMORY_BUDGET_MB` | `1024` | |
| `MAX_RASTER_PIXELS` | `100000000` | |
| `MAX_DECODED_BYTES` | `1048576` | |
| `MAX_URL_BYTES` | `16384` | |
| `MAX_LAYOUT_WORK_UNITS` | `40000000` | Sized for 500 edges |
| `FONT_DIR` | `/usr/share/mermaid-ink/fonts` | |
| `CACHE_MEMORY_MB` | `25` | |
| `REDIS_URL` | unset | |
| `REDIS_TTL_SECS` | `604800` | |
| `REDIS_TIMEOUT_MS` | `150` | |
| `HTTP_CACHE_MAX_AGE` | `604800` | |
| `REMOTE_IMAGES` | `true` | |
| `REMOTE_IMAGE_TIMEOUT_MS` | `2000` | |
| `REMOTE_IMAGE_MAX_BYTES` | `5242880` | |
| `REMOTE_IMAGE_MAX_COUNT` | `10` | |
| `RUST_LOG` | `info` | |

Every variable fails at boot on an unparseable value, except `MAX_WIDTH`/`MAX_HEIGHT`, which keep JS semantics.

## Milestones

1. Done: golden and vector capture tooling, plus fixtures recorded from the Node reference (mermaid 12.3.0).
2. Done: foundation: decode, query, HTTP contract, CORS and errors, plus the vector tests and ported unit tests.
3. Done: rendering: merman for svg, png, jpeg, webp and pdf; fonts; FA; svg post-processing; security; kitchensink port.
4. Done: caches: LRU, Redis, keys, HTTP headers.
5. Done: remote image fetcher.
6. In progress: parity green (strict by default), hardening, Docker (distroless), CI, full README rewrite. The Node code is already out of the tree.
