//! Request semantics shared by every crate: decoding `:encodedCode`, query parsing with the
//! exact JS semantics of the legacy service, cache keys, and types passed between crates.
//! Ported from legacy/src/helpers/*.js; correctness is defined by tests/fixtures/vectors.

pub mod decode; // port of getOptionsFromCode.js (+ renderSVG.js state/config preparation)
mod js; // parseInt/parseFloat/ToNumber/String(number)/JSON.parse with JS semantics
pub mod key; // port of createCacheKey.js (getQueryKey + sha256 key)
pub mod query; // port of utils.js extract*/validateQuery/isLandscape/isFit

/// A remote image fetched by `mink-fetch` and embedded by `mink-render`.
#[derive(Clone, Debug)]
pub struct FetchedImage {
    pub bytes: bytes::Bytes,
    /// "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}
