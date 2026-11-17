# Golden and vector capture (reference: the legacy Node/Chrome service)

The fixtures in `tests/fixtures/` are already recorded; you only need this to re-record them.
The Node reference is not in this repo. It is upstream jihchi/mermaid.ink at commit 588671e with `mermaid` bumped to 12.3.0 (pnpm lockfile: mermaid 12.3.0, elkjs 0.12.1, @zenuml/core 3.49.2).
To re-record:
1. Check out that state into `legacy/`.
2. Build the image: `docker build -t mink-golden legacy`.
3. Run the scripts below. They read `legacy/test/kitchensink.test.js` and `legacy/src/static/index.html`.


## Vectors

`gen-vectors.mjs` records what the reference JS helpers return (`src/helpers/getOptionsFromCode.js`,
`utils.js`, `createCacheKey.js`) into `tests/fixtures/vectors/*.json`. The Rust code must match them exactly.
The script runs inside the `mink-golden` image, so it uses that image's Node, js-base64 and pako.
It needs no npm packages on the host.

Regenerate (Git Bash, from the repo root):

```bash
MSYS_NO_PATHCONV=1 docker run --rm --entrypoint node -e GEN_DATE=2026-11-17 \
  -v "$(pwd -W):/host:ro" -v "$(pwd -W)/tools/golden:/gen:ro" \
  -v "$(pwd -W)/tests/fixtures/vectors:/out" mink-golden /gen/gen-vectors.mjs
```

- The helpers are imported from the image WORKDIR (`REPO`, default `/home/pptruser`).
- The image's `test/` directory is empty, so the encoded literals are read from the host checkout mounted at `/host` (`LIT_ROOT`).
- `GEN_DATE` only goes into `meta.json`. With the same date, two runs produce byte-identical files. The fuzz uses mulberry32 with seed `0x5EED`.
- Each file is one JSON object. Its header keys come first, then an array with one entry per line.

| File | Contents |
|---|---|
| `decode.json` | `getOptionsFromCode(input)`. `input` is the already-`decodeURIComponent`'d path param. `src` is `kitchensink`, `index.html`, `edge` or `fuzz` (4000 seeded mutations). `outcome` is `ok`, `unknown_serde` or `error`. `value` is `JSON.stringify` of the result, so it is lossy for `Infinity` (becomes `null`) and `-0` (becomes `0`). |
| `query.json` | The full cross product of width × height × scale tokens × states for `validateQuery` and `extractDimension`. Rows are index arrays (see the `format` key). Numbers are `{s: String(n), bits: Float64 hex}` in the `numbers` table. A token of `""` means the key is absent. |
| `bg_color.json`, `theme.json`, `paper.json`, `image_type.json` | `{in, out}` or `{in, error}`. A missing `in` means the param is absent. An array `in` is a repeated query param. |
| `flags.json` | `isLandscape` / `isFit`. |
| `query_key.json` | `getQueryKey(assetType, query, options)` for img/svg/pdf. A missing `query`/`options` key means `undefined`. |
| `cache_key.json` | `createCacheKey(args)` as hex (or `""`). The script asserts the two constants from `test/createCacheKey.test.js`. |
| `meta.json` | The Node, js-base64 and pako versions, the date and the seed. |

Behaviours worth knowing when porting (all recorded in the vectors):
- The fallback path decodes with Node `Buffer` base64, which stops at the first `=`. `ab==cd` gives `"i"`.
- The js-base64 path strips every non-alphabet character. `!!eyJjb2RlIjoiYSJ9` decodes fine.
- A prefix is checked with `in`, so inherited `Object.prototype` keys count as prefixes:
  - `hasOwnProperty:x` and `isPrototypeOf:x` → `false`.
  - `constructor:1` → `1`.
  - `valueOf:`, `toString:` and `__proto__:` throw inside the deserializer and fall back to base64-decoding the whole string.
- Only the fallback path checks for `code`. Valid base64/pako JSON without `code` is returned as is, for example `{"mermaid":"{}"}`, `null`, `[]` or `"s"`.
- A BOM is kept on the base64 path (JSON.parse fails, so the fallback gives `code` = the raw JSON text). On the pako path the BOM is stripped (TextDecoder).
- `extractBgColor`'s regex is unanchored on one side. `zzzzabc` → `#zzzzabc`, and `!red;background:url(x)` → `red;background:url(x)`.
- `toLowerCase` maps the Kelvin sign: `darK` is the `dark` theme.
- An array-valued width parses (`parseInt(['1','2'])` → 1). An array-valued bgColor, theme, paper or type throws `... is not a function`.
- `getQueryKey` with `{}` (what the app passes) gives `width=null,height=null,...`. With `undefined` it does not.
- Without `scale`, the max limits are not enforced: width 10001 with max 10000 stays 10001.
- `decode.json` has 19 values with a lone surrogate (`"\ud800"`). JSON.parse accepts these, but serde_json rejects them by default. The Rust decoder and the vector test must decide how to handle them.

## Goldens

Recorded responses of the Node/Chrome reference (image `mink-golden`: mermaid 12.3.0, elkjs 0.12.1,
@zenuml/core 3.49.2, FA 7.2.0, Chrome for Testing 157.0.8103.50, Node v24.22.2; exact values in
`tests/fixtures/golden/meta.json`). Rust tests read these files; no Node is needed at test time.

Rerun everything (Git Bash or any POSIX shell, from the repo root, Docker running, host `node` >= 22 on PATH for `encode.mjs`/`capture.mjs`, host ports 3101-3104 free, ~50 min):

```bash
bash tools/golden/run.sh            # IMAGE=other-tag bash tools/golden/run.sh to use another image
```

`run.sh` runs, in order:
1. `node tools/golden/encode.mjs`: writes `tools/golden/corpus/k-series.json` and `tests/fixtures/corpus/encoded.json`.
2. Starts `mink-ref-main` (:3101, `QUEUE_ADD_TIMEOUT=60000 QUEUE_CONCURRENCY=2`), `mink-ref-timeout` (:3102, `QUEUE_ADD_TIMEOUT=1`),
   `mink-ref-maxw` (:3103, `MAX_WIDTH=1005`) and `mink-ref-fa` (:3104, `FONT_AWESOME_CSS_URL=https://example.com/ajax/libs/font-awesome/FA_VERSION/css/all.min.css`), all with `--cap-add=SYS_ADMIN` (Chrome's sandbox).
3. `node tools/golden/capture.mjs`: waits for `GET /` on each instance, then sends every case twice (2 at a time). `--dry` only prints the case count.
4. `pdftoppm -r 72 -singlefile` (page 1) for every captured PDF, in a throwaway `debian:bookworm-slim` container (needs network for apt).
5. `tools/golden/meta.mjs`, run inside the reference image, writes `meta.json`.
6. Removes the containers (also on failure, via `trap`).

### Corpus (`tools/golden/corpus/`)

One `<id>.mmd` per diagram, plus an optional `<id>.config.json` (the `mermaid` config object; the default is `{"theme":"default"}`).

- **K-series**: every `/img|/svg|/pdf/...` literal in `test/kitchensink.test.js` and `src/static/index.html`, stored verbatim in `k-series.json`.
  - `encoded`: one id per distinct encoded string (K01–K24).
  - `literals`: each literal with its `source` line, `method`, `raw` text, `resolved` text, endpoint and query.
  - Template literals are resolved the way the test sends them: `encodedPath` becomes its constant, `MAX_WIDTH + 1` becomes `NaN` (the env var is unset), and `diagramSize` becomes 1000/800.
  - Test titles such as `test('/img/:code')` are skipped. `&amp;` in index.html is unescaped.
- **T-series** (all 200 on every endpoint):

  | Id | Diagram |
  |---|---|
  | T01 | flowchart TD |
  | T02 | flowchart LR with subgraphs, classDef, linkStyle and click href |
  | T03 | `@{shape}` shapes and an image shape with an inline `data:image/png` |
  | T04 | front-matter `layout: elk` |
  | T05 | `flowchart-elk` |
  | T06 | `htmlLabels: false` |
  | T07 | markdown strings |
  | T08 | sequence |
  | T09 / T10 | classDiagram / classDiagram-v2 |
  | T11 / T12 | stateDiagram / stateDiagram-v2 (tall) |
  | T13 | ER |
  | T14 | journey |
  | T15 | gantt with fixed dates and `todayMarker off` |
  | T16 | pie showData |
  | T17 | quadrant |
  | T18 | requirement |
  | T19 / T19w | gitGraph / 60 commits LR (very wide) |
  | T20–T24 | C4 Context, Container, Component, Dynamic, Deployment |
  | T25 / T26 | mindmap / mindmap with tidy-tree |
  | T27 | timeline |
  | T28 | zenuml |
  | T29 | sankey |
  | T30 | xychart |
  | T31 | block |
  | T32 | packet |
  | T33 | kanban |
  | T34 | architecture |
  | T35 | radar |
  | T36 | treemap |
  | T37 | info |

  T38–T42 are venn-beta, ishikawa, wardley-beta, railroad-beta and usecase-beta. Mermaid 12.3 supports all five, so they are 200.
- **F-series**:

  | Id | Case | Result |
  |---|---|---|
  | F01 | `%%{init}%%` forest | |
  | F02 | front-matter title and config | |
  | F03 | handDrawn, `handDrawnSeed: 42` | |
  | F04 | themeVariables | |
  | F05 | `securityLevel: strict` | forced back to loose |
  | F06 | emoji | |
  | F07 | CJK | |
  | F08 | Arabic and Hebrew | |
  | F09 | `fa:` / `fab:` icons | |
  | F10 | 3000-char label | |
  | F11 | 55k chars (`style` lines) | 200, red "Maximum text size" box. Padding with `%%` comments does NOT trigger it, because comments are stripped first. |
  | F12 | F11 with `maxTextSize: 100000` | real render |
  | F13 | 510 edges | 400, `Edge limit exceeded. 500 edges found, but the limit is 500.` |
  | F14 | syntax error | 400 |
  | F15 / F16 / F17 | empty, comment-only, `hello world` | 400 `Unknown diagram error`. F15's bare-base64 form is `/svg/`, which is 404. |
  | F18 | `mermaid` as an object | 200 |
  | F19 | `mermaid: null` | 200, but `?theme=dark` gives 400 `Cannot set properties of null (setting 'theme')` |
  | F20 | `mermaid: '{bad json'` | 400, JSON.parse message |
  | F21a | ER label with `<img onerror>` | 200, stripped |
  | F21b | `<script>`, `onmouseover` and `<iframe>` labels | 200, stripped by mermaid's DOMPurify |
  | F21c | `click ... javascript:` | 200. Both `javascript:alert(n)` hrefs are KEPT in the SVG (securityLevel loose). |

### Cases (`tools/golden/matrix.json` → `tests/fixtures/golden/`)

- `encoded.json` has, per T/F item:
  - `pako`: zlib level 9, base64url without padding.
  - `base64`: `base64:` + base64url of `{code, mermaid: "<config JSON string>"}`.
  - `plain`: base64url of the raw code.

  Only `pako` gets the parameter matrix. `base64` and `plain` get one `/svg` case each. Identical requests are captured once, so F04.plain is the same as F03.plain, and so on.
- Case id: `<corpus>__<endpoint>__<slug(query)>`.
  - In the slug, `=` becomes `-`, `&` becomes `.`, and other characters become `_`.
  - Ids that collide case-insensitively (Windows) get `~<sha6>`.
  - D-series ids are `Dnn__<name>`.
  - Env-variant cases end in `__env-<instance>`.
- Every request carries `Origin: https://parity.test`, except D23/D25 (no Origin) and the kitchensink CORS literals (`http://cross.origin.com` + ACRM).
- Files:
  - `manifest.json`: every case's id, env, method, path, query, request headers, corpus and source.
  - `cases/<id>.json`: `{status, headers (minus date/connection/keep-alive), body_len, body_sha256, body_file, error_text, nondeterministic}`. Also `second` when the two responses differed, and `nondeterministic_ignoring_pdf_dates` for PDFs.
  - `bodies/<id>.<ext>`: the first response body.
  - `pdfpng/<id>-p1.png`: page 1 at 72 dpi.
  - `meta.json`.

Counts from the 2026-11-17 capture (755 cases; 43 MB, so nothing was downscaled):

| Endpoint | Statuses |
|---|---|
| /svg | 200: 316, 204: 2, 400: 43, 404: 3, 431: 3, 500: 9, 503: 1 |
| /img | 200: 177, 204: 1, 400: 13, 404: 2, 503: 1 |
| /pdf | 200: 172, 400: 7 |
| other (`/`, `/foo`) | 200: 3, 204: 1, 404: 1 |

Known nondeterministic cases (313; the full list is the `nondeterministic: true` records):
- `nondeterministic` is also forced to true, even when both responses matched, for every PDF and for every SVG with a gantt `class="today"` line (K07). Both carry the wall clock, so a later run always differs. In the 2026-11-17 capture, 44 PDFs matched only because both requests fell in the same second (an audit rerun confirmed they differ). `second` is still written only when the two responses actually differed.
- **PDF**: all 172 are flagged. 128 differed within the run, 87 of them only in `/CreationDate` and `/ModDate`. Use `nondeterministic_ignoring_pdf_dates`: with the dates masked, PDFs reproduce across runs.
  - The other 41 are the subjects below, plus every T12 (tall state) PDF.
- **SVG**: 60 differ within the run, plus K07 (today line, whose x moves every day). For K05, K09, T02 and T03 the cause was checked: it is the rough.js path jitter mermaid uses for some shapes (the seed is random unless `handDrawnSeed` is set). The others were not inspected.
  - Subjects: K05, K09, K22, K23 (class/ER kitchensink), F21a, T02, T03, T09, T10, T12, T13, T18, T19 and T34.
  - Every T12 /svg case is affected.
- **Raster**: 80 /img cases differ: the jittery subjects above, plus Chrome screenshot variance on most default-JPEG renders and on all T03 renders.
  - Treat raster goldens as SSIM-only.

Reference behaviours worth knowing (all recorded):
- No `Origin` gives `Access-Control-Allow-Origin: *`. `Vary: Origin` is on every response.
- Preflight (`OPTIONS` + ACRM) is 204 on any path, and mirrors `Access-Control-Request-Headers`.
- `OPTIONS` without ACRM is 404 with no ACAO.
- A URL over Node's 16 KB header limit gives 431 with no headers and an empty body (no CORS).
- A bad percent-escape (`/svg/%E0%A4%A`) is 500.
- `HEAD` renders and returns GET's headers with an empty body. `POST` is 404.
- A repeated `bgColor` or `theme` is 500. A repeated `width` is 200: `parseInt` of an array takes the first value.
- `PAKO:` is 400 `Unknown serde type: PAKO`.
- Pako input is accepted with the standard alphabet (`%2B` / `%2F`), with `=` padding, and gzip-wrapped. Raw deflate and truncated input are 400 `Unknown diagram error`.
- `QUEUE_ADD_TIMEOUT=1` gives 503 `Service Unavailable` (no Retry-After) on renders. A validation error on that instance still came back 400 in this capture; it races the timeout.
- With `MAX_WIDTH=1005`:
  - `width=1000&scale=1.005` passes validation (1005). The SVG keeps width 1000px, because extractDimension rounds the scale to 1.01, which gives 1010 > max.
  - `scale=1.006` is 400.
  - `width=2000` without scale stays 2000px.
