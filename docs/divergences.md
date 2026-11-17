# Differences from upstream mermaid.ink

This fork keeps every URL, encoding, query parameter, default and status code of the Node/Puppeteer service.
`tests/parity.rs` replays 755 recorded responses from the old service. Every allowed difference is listed in `tests/fixtures/divergences.toml` with its reason.

## Deliberate behaviour changes

| Area | Legacy (Node/Chrome) | This fork |
|---|---|---|
| Repeated query params (`?bgColor=a&bgColor=b`) | 500 for bgColor/theme/type/paper | The first value is used |
| 503 responses | `Retry-After` set, then stripped by Koa | `Retry-After: 1` is sent |
| Caching headers | None | `Cache-Control: public, max-age=HTTP_CACHE_MAX_AGE` + weak `ETag` on 200/400; `If-None-Match` → 304 |
| /svg security | `javascript:` links kept | `on*` attributes, `<script>`, `javascript:`/`vbscript:`/`data:text/html` URLs stripped; `Content-Security-Policy` header |
| Request target 16 KiB–64 KiB | 431 | 431 (same) |
| Request target > 64 KiB | 431 | 414 (rejected by the HTTP parser) |
| Decoded payload size | Unbounded | 400 `Decoded diagram is too large` above `MAX_DECODED_BYTES` |
| Raster size | Any size (`?width=20000` rendered) | 400 above `MAX_RASTER_PIXELS`, or when the job exceeds `RENDER_MEMORY_BUDGET_MB` |
| JPEG transparent areas | White or black at random | Always white |
| Mermaid parse / limit errors | 400 with Mermaid's jison text | 400 with merman's message (`Unknown diagram error` unchanged) |
| Concurrency | 1 render at a time by default | `QUEUE_CONCURRENCY` defaults to the CPU count; `QUEUE_MAX_PENDING` sheds load with an immediate 503 |
| Remote images | Fetched by Chrome without restrictions | https only, SSRF-safe, size and time limits, can be disabled (`REMOTE_IMAGES=false`); SVG images refused |
| Cache | Optional Postgres | In-memory LRU (`CACHE_MEMORY_MB`) + optional Redis (`REDIS_URL`) |

## Rendering differences

Rendering uses merman, a native Rust port of Mermaid, instead of Chrome, so pixels are not identical.

- **Matched exactly:** status codes, headers, our own error texts, explicitly requested dimensions, label text, and node/edge/cluster counts.
- **Compared with tolerances:** other dimensions (±4%) and image similarity (SSIM ≥ 0.90 raster, ≥ 0.85 PDF).
- **Known gaps** (ZenUML width, sequence spacing, small-text baseline, a few newer diagram types) are listed in `tests/fixtures/divergences.toml`. The upstream issue texts are in `docs/upstream-issues.md`.
