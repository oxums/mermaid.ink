# Upstream issues to file against merman (not filed)

These are fidelity gaps in merman 0.10.0 that `tests/fixtures/divergences.toml` currently tolerates.
Each issue compares Mermaid 12.3.0 in Chrome (the reference) against merman 0.10.0, using diagrams from `tools/golden/corpus/`.
Once an issue is fixed upstream, remove the matching divergence entry and run `cargo test --test parity`.

1. **ZenUML layout narrower than @zenuml/core.** Repro: `tools/golden/corpus/T28.mmd`. Expected viewBox 935x624 (@zenuml/core 3.49.2); got 883.26x628. Participant and group widths are smaller.
2. **Sequence diagram spacing.** Repro: `T08.mmd` (autonumber, loop/alt/par, notes, box).
   - Message rows are 1-2 px taller (text box height 18 vs Chrome's 17).
   - The ~30 px bottom margin under mirrored actors is missing.
   - Actors are spaced wider.
   - Result: /pdf?fit page height is 790.5 pt vs 798.96 pt, and ledger / tabloid-landscape output fits on 1 page instead of 2.
3. **Stylesheet omits a theme rule.** packet-beta, radar-beta, venn-beta, ishikawa and railroad-beta output has no CSS rule using `#9370db`, which Mermaid emits. Rendering is unaffected.
4. **Class diagram note link drawn solid.** Repro: `T09.mmd`. The edge with classes `edge-pattern-dotted edge-thickness-normal relation` has no effective dash array.
5. **Kanban card text alignment.** Repro: `T33.mmd`. The resvg text fallback ignores the inner span's `text-align:left !important`, so multi-line card text is centered.
6. **Raster text baseline and fallback.** Small text-heavy diagrams (e.g. `F10.mmd`, K12) score SSIM 0.83-0.89 against Chrome, for three reasons:
   - Text in the resvg fallback sits about 1 px lower (half-leading and ascent rounding at line-height 1.5).
   - Font fallback is chosen per line rather than per glyph.
   - `fa:` icons do not get their 1.25em inline box at layout time.
7. **block-beta / venn-beta / wide gitGraph geometry.** Repros: `T31.mmd`, `T38.mmd`, `T19w.mmd`. Element placement differs from Mermaid 12.3.
