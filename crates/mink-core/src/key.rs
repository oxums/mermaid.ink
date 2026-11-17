//! Cache keys: a port of legacy/src/helpers/createCacheKey.js.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::js;
use crate::query::{
    Limits, Query, extract_bg_color, extract_dimension, extract_image_type, extract_paper,
    extract_theme, is_fit, is_landscape,
};

/// `getQueryKey(assetType, query, options)`. A `None` query is JS `undefined`; the server always
/// has a query (possibly empty), which yields `width=null,height=null` like Koa's `ctx.query`.
pub fn get_query_key(asset_type: &str, query: Option<&Query>, limits: &Limits) -> String {
    let empty = Query::default();
    let q = query.unwrap_or(&empty);
    let mut parts = Vec::new();
    if let Some(bg) = extract_bg_color(q) {
        parts.push(format!("bgColor={bg}"));
    }
    if let Some(d) = extract_dimension(query, limits) {
        let show = |v: Option<f64>| v.map_or_else(|| "null".to_owned(), js::number_to_string);
        parts.push(format!("width={}", show(d.width)));
        parts.push(format!("height={}", show(d.height)));
    }
    if let Some(theme) = extract_theme(q) {
        parts.push(format!("theme={theme}"));
    }
    match asset_type {
        "img" => parts.push(format!("type={}", extract_image_type(q))),
        "pdf" => {
            parts.push(format!("paper={}", extract_paper(q)));
            if is_landscape(q) {
                parts.push("landscape=true".into());
            }
            if is_fit(q) {
                parts.push("fit=true".into());
            }
        }
        _ => {}
    }
    parts.join(",")
}

/// `createCacheKey(...)` as lowercase hex of sha256(`asset | code[ | queryKey]`), or "" when
/// `asset_type` or `encoded_code` is empty (legacy's `assetType` defaults to "img").
pub fn create_cache_key(
    asset_type: &str,
    encoded_code: &str,
    query: Option<&Query>,
    limits: &Limits,
) -> String {
    if asset_type.is_empty() || encoded_code.is_empty() {
        return String::new();
    }
    let mut raw = format!("{asset_type} | {encoded_code}");
    let query_key = get_query_key(asset_type, query, limits);
    if !query_key.is_empty() {
        raw.push_str(" | ");
        raw.push_str(&query_key);
    }
    Sha256::digest(raw)
        .iter()
        .fold(String::with_capacity(64), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}
