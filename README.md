# mermaid.ink (Rust fork)

An HTTP service that turns a URL into a rendered [Mermaid](https://mermaid.js.org) diagram, as SVG,
PNG, JPEG, WebP or PDF.

This is a hard fork of [jihchi/mermaid.ink](https://github.com/jihchi/mermaid.ink), rewritten in
Rust. Diagrams are parsed, laid out and rendered natively by [merman](https://github.com/Latias94/merman)
(a Rust port of Mermaid 12.3), then rasterized with resvg. There is no browser, Node.js, Puppeteer
or Chrome, and no database. The URL format, query parameters and error responses are the same as
upstream's, so existing links keep working. The deliberate differences are listed in
[Differences from upstream mermaid.ink](#differences-from-upstream-mermaidink).

## Quick start

### Cargo

```bash
bash tools/fonts/fetch.sh            # once: downloads the pinned font set into ./fonts (needs Docker)
FONT_DIR=fonts cargo run --release   # http://localhost:3000
```

### Docker

```bash
docker build -t mermaid-ink .
docker run --rm -p 3000:3000 mermaid-ink
```

The image is `gcr.io/distroless/cc-debian12:nonroot` with the binary and the fonts, and needs no
extra capabilities or seccomp profile. Tagged releases are pushed to `ghcr.io/<owner>/<repo>` by CI.

Then open http://localhost:3000, or try
http://localhost:3000/svg/pako:eNqrVkrOT0lVslJKL0osyFDwCYrJU1BwjM5NLcpNzEzRy8zLjlXQ1bVTcNIIKi0u0VTSUYJKKVlVK5VkpOaC9KakpiWW5pQo1dYCAPW8Gcw

## API

| Endpoint | Returns |
|---|---|
| `GET /` | The home page (usage and examples). |
| `GET /svg/:encoded` | `image/svg+xml` |
| `GET /img/:encoded` | `image/jpeg` (default), `image/png` or `image/webp`, chosen by `?type` |
| `GET /pdf/:encoded` | `application/pdf` |

- Routes are case-insensitive and accept one trailing slash.
- `HEAD` is answered like `GET`, without a body.
- CORS: `Access-Control-Allow-Origin` mirrors the request's `Origin` (or `*`), with `Vary: Origin`. A preflight `OPTIONS` returns 204 on any path.
- Unknown paths and other methods return 404.

### `:encoded`

The path segment holds the diagram, in one of these forms:

| Form | Example | Contents |
|---|---|---|
| `pako:<data>` | `pako:eNpLL0os...` | URL-safe base64 of a zlib (or gzip) deflated JSON state, as produced by [mermaid.live](https://mermaid.live) |
| `base64:<data>` | `base64:eyJjb2RlIjoi...` | base64 (standard or URL-safe) of the JSON state |
| bare base64 JSON | `eyJjb2RlIjoi...` | base64 of the JSON state, no prefix |
| bare base64 code | `Z3JhcGggVEQ...` | base64 of the diagram source itself |

The JSON state is `{"code": "<diagram source>", "mermaid": <config>}`. `mermaid` is the Mermaid
configuration, either as an object or as a JSON string (for example `"{\"theme\":\"dark\"}"`).
Bare base64 code gets `{"theme":"default"}`. An unknown `prefix:` returns 400
`Unknown serde type: prefix`.

Decoding follows upstream's JavaScript exactly, quirks included (see `tests/fixtures/vectors`).
The inflated state is capped at `MAX_DECODED_BYTES`.

### Query parameters

All parameters are optional. Invalid values that have a validation message return 400. Other
invalid values are ignored and the default is used.

| Param | Endpoints | Default | Meaning |
|---|---|---|---|
| `theme` | all | the diagram's own | `default`, `neutral`, `dark` or `forest` (case-insensitive). Overrides the theme in the state's config. Other values are ignored. |
| `bgColor` | all | transparent (JPEG: white) | Background colour. Hex without `#` (`FF0000`, `0f08`), or a named colour prefixed with `!` (`!white`). |
| `width` | all | natural size | Width in px. Must be a positive integer, otherwise 400 `invalid width value`. |
| `height` | all | natural size | Height in px. Must be a positive integer, otherwise 400 `invalid height value`. |
| `scale` | all | `1` | Multiplier from 1 to 3 (rounded to 2 decimals) applied to `width`/`height`. Needs `width` or `height`, otherwise 400. The scaled size must not exceed `MAX_WIDTH`/`MAX_HEIGHT`, otherwise 400. |
| `type` | `/img` | `jpeg` | `jpeg`, `png` or `webp`. Other values give JPEG. |
| `paper` | `/pdf` | `a4` | `letter`, `legal`, `tabloid`, `ledger`, `a0` to `a6`. |
| `landscape` | `/pdf` | off | Present (any value, even `false`) means landscape paper. |
| `fit` | `/pdf` | off | Present (any value) means the page is sized to the diagram, with no paper and no pagination. |

Without `width`/`height`, the diagram is laid out in a 1904 px wide box, the same as the
upstream service's Chrome viewport. Raster output is at 1x. JPEG uses quality 90 and WebP uses
quality 90. A WebP side over 16383 px returns 400. PDFs paginate tall diagrams over several pages.

When a query parameter is repeated, the first value is used.

### Responses

| Status | When | Body |
|---|---|---|
| 200 | Rendered | the image |
| 304 | `If-None-Match` matches the ETag | empty |
| 400 | Invalid query, undecodable state, Mermaid parse error, diagram over a limit | `text/plain` message |
| 404 | Unknown route | `Not Found` |
| 414 / 431 | Request target over 64 KiB / over `MAX_URL_BYTES` | empty |
| 500 | Malformed percent-encoding in the path, internal error, renderer panic | `Internal Server Error` |
| 503 | Overloaded, or the render deadline passed | `Service Unavailable`, with `Retry-After: 1` |

Code longer than Mermaid's `maxTextSize` (default 50000) renders Mermaid's
"Maximum text size in diagram exceeded" diagram with status 200, as Mermaid does.

## Configuration

Every variable is optional. An unparseable value stops the server at boot, except `MAX_WIDTH` and
`MAX_HEIGHT`, which keep upstream's JavaScript semantics. Empty values count as unset.

| Variable | Default | Description |
|---|---|---|
| `HOST` | `0.0.0.0` | Address to listen on. |
| `PORT` | `3000` | Port to listen on. |
| `MAX_CONNECTIONS` | `10000` | Client connections open at once. Further connections wait to be accepted. |
| `MAX_WIDTH` | `10000` | Largest scaled width (`width` × `scale`). Kept as a string and compared with JavaScript coercion, like upstream; never a boot error. |
| `MAX_HEIGHT` | `10000` | Largest scaled height. Same semantics as `MAX_WIDTH`. |
| `FONT_AWESOME_CSS_URL` | `https://cdnjs.cloudflare.com/ajax/libs/font-awesome/7.2.0/css/all.min.css` | Stylesheet `@import`ed by SVG output for `fa:` icons. The string `FA_VERSION` is replaced with `7.2.0`. |
| `QUEUE_CONCURRENCY` | CPU count | Renders running at once. |
| `QUEUE_ADD_TIMEOUT` | `3000` | Milliseconds a request may wait for a render slot, and the render deadline. Past either, 503. |
| `QUEUE_MAX_PENDING` | `256` | Requests waiting for a slot. Beyond this, requests get 503 immediately. |
| `RENDER_MEMORY_BUDGET_MB` | `1024` | Memory that renders running at once may use, in MiB. A single render larger than this is 400. |
| `MAX_RASTER_PIXELS` | `100000000` | Largest raster canvas (width × height). Larger is 400. |
| `MAX_DECODED_BYTES` | `1048576` | Largest decoded/inflated state, in bytes. Larger is 400 `Decoded diagram is too large`. |
| `MAX_URL_BYTES` | `16384` | Longest request target (path + query), in bytes. Longer is 431. |
| `MAX_LAYOUT_WORK_UNITS` | `40000000` | Layout work budget per diagram (enough for about 500 edges). Over it is 400. |
| `FONT_DIR` | `/usr/share/mermaid-ink/fonts` | Directory of `.ttf`/`.ttc`/`.otf` fonts, loaded once at boot in file-name order. |
| `CACHE_MEMORY_MB` | `25` | Size of the in-memory LRU cache in MiB. `0` disables it. |
| `REDIS_URL` | unset | Enables the Redis second-level cache, e.g. `redis://redis:6379` or `rediss://...`. |
| `REDIS_TTL_SECS` | `604800` | Expiry of Redis entries, in seconds (7 days). |
| `REDIS_TIMEOUT_MS` | `150` | Redis GET timeout. A timeout counts as a cache miss. |
| `HTTP_CACHE_MAX_AGE` | `604800` | `max-age` of the `Cache-Control` header on 200/400 responses. `0` omits the header. |
| `REMOTE_IMAGES` | `true` | Fetch `https` images referenced by diagrams. `true`/`1` or `false`/`0`. |
| `REMOTE_IMAGE_TIMEOUT_MS` | `2000` | Total time allowed per remote image. |
| `REMOTE_IMAGE_MAX_BYTES` | `5242880` | Largest remote image, in bytes (5 MiB). |
| `REMOTE_IMAGE_MAX_COUNT` | `10` | Most remote images fetched per diagram. |
| `RUST_LOG` | `info` (renderer warnings off) | [`tracing` filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html), e.g. `mermaid_ink=debug`. Diagram code and URLs are only logged at debug level. |

## Caching

- **In memory (L1):** an LRU cache bounded by bytes (`CACHE_MEMORY_MB`, default 25 MiB). It stores
  200 and 400 responses, never 5xx or renders where a remote image failed. Concurrent requests for
  the same uncached diagram are coalesced into one render.
- **Redis (L2, optional):** set `REDIS_URL`. Entries are stored under `mink:v1:<key>` with
  `REDIS_TTL_SECS` expiry, so rendered diagrams survive restarts and are shared between replicas.
  Startup never waits for Redis, writes are fire-and-forget, and any Redis error or a GET slower than
  `REDIS_TIMEOUT_MS` is treated as a miss.
- **HTTP:** 200 and 400 responses carry `Cache-Control: public, max-age=<HTTP_CACHE_MAX_AGE>` and a
  weak `ETag`. `If-None-Match` returns 304.

The cache key covers the endpoint, the decoded state, the relevant query parameters, the limits,
and a fingerprint of everything that changes output bytes: the merman version, the font files,
the FontAwesome URL, `REMOTE_IMAGES` and the layout budget. Changing any of these never serves a
stale render.

## Remote images

Diagrams can reference images (image shapes, `<img>` in labels). With `REMOTE_IMAGES=true` (the
default) the server fetches them before layout, so their natural size is used. `/svg` keeps the
original URL; PNG, JPEG, WebP and PDF output embed the image.

The fetcher treats every URL as hostile:

- `https` on port 443 only. No proxies. At most 3 redirects, each one checked again.
- DNS is resolved by the server (1 s timeout), and every address must be public. The connection
  goes to exactly the checked addresses, so DNS rebinding cannot reach an internal host. Loopback,
  private, link-local (including the 169.254.169.254 metadata endpoint), CGNAT, multicast,
  documentation, reserved and unspecified ranges are refused, also when wrapped in IPv4-mapped,
  NAT64, 6to4 or Teredo IPv6 addresses. IPv6 is only allowed in `2000::/3`.
- Timeouts: connect 1 s, read 1 s, `REMOTE_IMAGE_TIMEOUT_MS` per image, plus a deadline for all
  fetches of a request.
- At most `REMOTE_IMAGE_MAX_BYTES` per image, enforced while downloading. The `Content-Type` and the
  file's magic bytes must both be PNG, JPEG, GIF or WebP. SVG is refused.
- At most `REMOTE_IMAGE_MAX_COUNT` images per diagram, 4 concurrent fetches per host and 32 in total.
  Results, including failures, are cached for 60 s.

A failed image renders as an empty box, and that response is not cached. Set
`REMOTE_IMAGES=false` to never make outbound requests: images then render as empty boxes.

## Limits and memory

Every render runs on a blocking thread pool behind three gates:

1. A wait queue of `QUEUE_MAX_PENDING`. When it is full, requests get 503 at once.
2. `QUEUE_CONCURRENCY` render slots. Waiting longer than `QUEUE_ADD_TIMEOUT` gives 503.
3. A memory budget of `RENDER_MEMORY_BUDGET_MB`. After layout, the server knows the real canvas
   size and reserves that render's memory (SVG work, plus about 8 bytes per pixel for PNG/JPEG,
   16 for WebP, 64 MiB for PDF) before rasterizing. A render that alone exceeds the budget is 400.

Each render also has a deadline (`QUEUE_ADD_TIMEOUT`, 503), a layout work budget
(`MAX_LAYOUT_WORK_UNITS`, 400) and a pixel cap (`MAX_RASTER_PIXELS`, 400). Requests are bounded by
`MAX_URL_BYTES`, `MAX_DECODED_BYTES`, and a 10 s limit for receiving request headers. A panic in the
renderer becomes a 500 for that request only.

Rough memory sizing: the fonts take about 35 MB, plus `CACHE_MEMORY_MB`, plus up to
`RENDER_MEMORY_BUDGET_MB` under load. With the defaults, plan for about 1.1 GB at peak; lower
`RENDER_MEMORY_BUDGET_MB` and `MAX_RASTER_PIXELS` on small machines.

## Differences from upstream mermaid.ink

The renderer is merman, not Mermaid running in Chrome, so pixels differ slightly: text
measurement, anti-aliasing and encoder output (PNG and PDF files are smaller). Other deliberate
changes:

- `/svg` responses carry a `Content-Security-Policy`, and `on*` attributes, `<script>` and
  `javascript:` links are removed.
- Repeated query parameters use the first value (upstream answered 500).
- `QUEUE_CONCURRENCY` defaults to the CPU count (upstream: 1). A full queue answers 503 with
  `Retry-After: 1`.
- Caps on the URL (`MAX_URL_BYTES`) and on the decoded state (`MAX_DECODED_BYTES`).
- Mermaid parse errors are 400 with merman's message text.
- `Cache-Control`, `ETag` and 304 on rendered responses. Caching uses memory and Redis, not
  PostgreSQL.
- JPEG output paints transparent areas white.
- Diagram types that only merman supports also render.
- `PROTOCOL_TIMEOUT`, `HEADLESS_MODE`, `ENABLE_DATABASE` and the `PG*` variables are gone.

The full list, with the reasons, is in [docs/divergences.md](docs/divergences.md) and
[tests/fixtures/divergences.toml](tests/fixtures/divergences.toml).

## Development

The workspace is the server package (`src/`) plus four crates:

| Crate | Role |
|---|---|
| `crates/mink-core` | `:encoded` decoding, query parsing with JavaScript semantics, cache keys |
| `crates/mink-render` | merman rendering to SVG/PNG/JPEG/WebP/PDF, fonts, FontAwesome glyphs, SVG post-processing |
| `crates/mink-fetch` | the hardened remote image fetcher |
| `crates/mink-cache` | the LRU cache and the optional Redis L2 |

```bash
bash tools/fonts/fetch.sh                                # fonts into ./fonts (tests need them)
cargo test --workspace                                   # all tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
RUST_LOG=mermaid_ink=debug FONT_DIR=fonts cargo run      # dev server on :3000
```

- **Redis tests** run when `REDIS_TEST_URL` is set:
  `docker run -d --rm -p 6390:6379 redis:7-alpine`, then
  `REDIS_TEST_URL=redis://127.0.0.1:6390 cargo test -p mink-cache`.
- **Parity:** `tests/parity.rs` replays the responses recorded from upstream jihchi/mermaid.ink
  (commit 588671e, with `mermaid` bumped to 12.3.0) in `tests/fixtures/golden`, and compares status,
  headers, error text, dimensions, labels, and raster/PDF similarity (SSIM). Run it with
  `cargo test --test parity -- --nocapture`. It is strict by default: renderer fidelity gaps not
  listed in `tests/fixtures/divergences.toml` fail the test. `PARITY_STRICT=0` only reports them.
- **Vectors:** `tests/fixtures/vectors` holds the exact outputs of the upstream JavaScript helpers
  (decoding, query parsing, cache keys). The Rust code must match them exactly.
- **tools/golden** holds the one-time Node scripts that recorded the goldens and vectors from the
  upstream image. They are not run in CI; see [tools/golden/README.md](tools/golden/README.md).
- **tools/fonts:** `fetch.sh` runs `install.sh` in `debian:bookworm-slim` to build the pinned font
  set (Liberation Sans, DejaVu, Loma, FreeFont, WenQuanYi Zen Hei, Noto Color Emoji, FontAwesome 7.2
  Free) and checks it against `tools/fonts/SHA256SUMS`. The Dockerfile uses the same script. The
  font files are part of the cache fingerprint, so the set must be byte-identical everywhere.

The design and its decisions are in [docs/PLAN.md](docs/PLAN.md). Notes for coding agents are in
[AGENTS.md](AGENTS.md).

## Licenses

mermaid-ink is MIT licensed ([LICENSE](LICENSE)). The binary and the Docker image include
components under other licenses, notably `merman-elk-layered` (EPL-2.0), the bundled fonts (GPL
with font exceptions, Bitstream Vera, SIL OFL 1.1) and libwebp (BSD-3-Clause). See
[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES).

## Contributors

Many thanks for your help!

<a href="https://github.com/jihchi/mermaid.ink/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=jihchi/mermaid.ink&max=11" />
</a>

The image of contributors is made with [contrib.rocks](https://contrib.rocks).
