# Agent Guide for mermaid.ink (Rust fork)

This is a hard fork being rewritten in Rust. Read `docs/PLAN.md`: it is the approved spec.
This is a standalone repo. The behaviour reference is upstream jihchi/mermaid.ink at commit 588671e with `mermaid` bumped to 12.3.0 (Node/Puppeteer); it is not in this tree. The fixtures in `tests/fixtures/` were recorded from it (see `tools/golden/README.md`).

## Layout

```
Cargo.toml              # workspace + the server binary package `mermaid-ink` (src/)
src/                    # axum server: config from env, routes, CORS, errors, admission, wiring
crates/mink-core        # decode (:encodedCode), query parsing (JS semantics), cache keys, shared types
crates/mink-render      # merman rendering -> svg/png/jpeg/webp/pdf, fonts, FA glyphs, svg post-processing
crates/mink-fetch       # hardened https image fetcher (SSRF-safe)
crates/mink-cache       # moka LRU (bytes) + optional Redis L2
static/index.html       # home page (embedded with include_str!)
tests/                  # server-level tests (kitchensink, parity, hardening) + tests/fixtures
tests/fixtures/vectors  # exact outputs of the upstream JS helpers: Rust must match exactly
tests/fixtures/golden   # recorded upstream HTTP responses: compared by the parity harness
tools/golden            # one-time Node capture scripts (never run in CI)
tools/fonts             # fetch.sh (docker) -> install.sh: pinned fonts into ./fonts, checked against SHA256SUMS
Dockerfile              # fonts stage + cargo build + distroless/cc-debian12:nonroot
```

## Commands

```bash
bash tools/fonts/fetch.sh              # once (needs Docker + network): fonts into ./fonts; tests need them
cargo build --release
cargo test --workspace                 # everything (fonts from ./fonts or FONT_DIR)
cargo test -p mink-core                # one crate
cargo test --test parity -- --nocapture                # parity vs tests/fixtures/golden (strict: fidelity gaps fail)
PARITY_STRICT=0 cargo test --test parity -- --nocapture # only report renderer fidelity gaps
REDIS_TEST_URL=redis://127.0.0.1:6390 cargo test -p mink-cache  # after: docker run -d --rm -p 6390:6379 redis:7-alpine
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
RUST_LOG=mermaid_ink=debug FONT_DIR=fonts cargo run   # dev server on :3000
docker build -t mermaid-ink .          # production image
```

If the font set changes on purpose, copy the new `fonts/SHA256SUMS` over `tools/fonts/SHA256SUMS`.

## Conventions

- Rust 2024 edition, stable toolchain. `cargo fmt`, and clippy must be clean.
- Errors: no `unwrap()` on request paths. Map failures to `RenderError`/HTTP status explicitly.
- Never set `panic = "abort"`: a render panic must become a 500.
- Logging: `tracing`. Never log diagram code or full URLs at info level.
- Compatibility: behaviour must match the upstream reference (the parity fixtures) unless `docs/PLAN.md` or `tests/fixtures/divergences.toml` lists a deliberate divergence.
- Keep dependencies to the ones pinned in `docs/PLAN.md`. Ask before adding others.
