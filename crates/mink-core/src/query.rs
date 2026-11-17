//! Query handling: a port of the legacy/src/helpers/utils.js extract*/validateQuery/isLandscape/isFit
//! helpers with the exact JS semantics (parseInt/parseFloat, truthiness, string coercion).

use crate::js;

/// The request query string, parsed like Koa's `ctx.query` (URLSearchParams).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    /// `raw` is the query string with or without its leading '?': '+' is a space, `%XX` escapes
    /// are decoded, invalid UTF-8 becomes U+FFFD, and a key without '=' has the value "".
    pub fn parse(raw: &str) -> Self {
        let raw = raw.strip_prefix('?').unwrap_or(raw);
        raw.split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (form_decode(k), form_decode(v))
            })
            .collect()
    }

    /// The first value of `key`. Repeated keys use the first value: a deliberate divergence
    /// (legacy turned them into arrays and mostly answered 500).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Query {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self {
            pairs: iter
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

fn form_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |i: usize| b.get(i).and_then(|&c| (c as char).to_digit(16));
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], hex(i + 1), hex(i + 2)) {
            (b'+', ..) => out.push(b' '),
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 2;
            }
            (c, ..) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A `ctx.state.maxWidth`-style limit with its JS type kept: legacy tested its truthiness,
/// compared numbers against it with coercion and printed it in messages.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum JsLimit {
    #[default]
    Undefined,
    Number(f64),
    String(String),
}

impl JsLimit {
    fn truthy(&self) -> bool {
        match self {
            Self::Undefined => false,
            Self::Number(n) => *n != 0.0 && !n.is_nan(),
            Self::String(s) => !s.is_empty(),
        }
    }

    fn number(&self) -> f64 {
        match self {
            Self::Undefined => f64::NAN,
            Self::Number(n) => *n,
            Self::String(s) => js::to_number(s),
        }
    }

    /// `!limit || v <= limit` (extractDimension).
    fn allows(&self, v: f64) -> bool {
        !self.truthy() || v <= self.number()
    }

    /// `limit && v > limit` (validateQuery).
    fn exceeded_by(&self, v: f64) -> bool {
        self.truthy() && v > self.number()
    }
}

impl std::fmt::Display for JsLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Undefined => f.write_str("undefined"),
            Self::Number(n) => f.write_str(&js::number_to_string(*n)),
            Self::String(s) => f.write_str(s),
        }
    }
}

/// `{maxWidth, maxHeight}` as legacy kept them in `ctx.state`. `Default` is both undefined.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub max_width: JsLimit,
    pub max_height: JsLimit,
}

impl Limits {
    /// `process.env.MAX_WIDTH ? process.env.MAX_WIDTH : 10000` (legacy/src/app.js): a non-empty
    /// env value stays a string, so `"0"` is truthy yet numerically 0 and `"abc"` never matches.
    pub fn from_env_values(max_width: Option<&str>, max_height: Option<&str>) -> Self {
        let limit = |v: Option<&str>| match v {
            Some(s) if !s.is_empty() => JsLimit::String(s.to_owned()),
            _ => JsLimit::Number(10000.0),
        };
        Self {
            max_width: limit(max_width),
            max_height: limit(max_height),
        }
    }
}

/// `{width, height}` from extractDimension; `None` is JS `null`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dimension {
    pub width: Option<f64>,
    pub height: Option<f64>,
}

/// A JS-truthy (non-empty) query value.
fn param<'a>(query: &'a Query, key: &str) -> Option<&'a str> {
    query.get(key).filter(|v| !v.is_empty())
}

/// `extractBgColor`: "!name" → "name", hex → "#hex". Keeps the regex quirk: only the "!" branch is
/// anchored at the start and only the hex branch at the end, so any value ending in 3 hex digits
/// (`zzzzabc`) or starting with "!" and a letter (`!red;background:url(x)`) passes.
pub fn extract_bg_color(query: &Query) -> Option<String> {
    let c = query.get("bgColor")?.trim_matches(js::is_ws);
    let named = c
        .strip_prefix('!')
        .is_some_and(|r| r.starts_with(|ch: char| ch.is_ascii_alphabetic()));
    let hex_tail = c.len() >= 3
        && c.as_bytes()[c.len() - 3..]
            .iter()
            .all(u8::is_ascii_hexdigit);
    if !named && !hex_tail {
        return None;
    }
    Some(match c.strip_prefix('!') {
        Some(name) => name.to_owned(),
        None => format!("#{c}"),
    })
}

/// `extractDimension`: `None` only when the query itself is absent (JS `undefined`).
/// Width/height of 0 or below are kept as parsed; validate_query rejects them first.
pub fn extract_dimension(query: Option<&Query>, limits: &Limits) -> Option<Dimension> {
    let q = query?;
    let int = |key| param(q, key).map(js::parse_int).filter(|v| v.is_finite());
    let (mut width, mut height) = (int("width"), int("height"));
    let truthy = |v: Option<f64>| v.is_some_and(|v| v != 0.0);
    let mut scale = 1.0;
    if let Some(s) = param(q, "scale")
        && (truthy(width) || truthy(height))
    {
        let s = js::parse_float(s);
        if s.is_finite() && (1.0..=3.0).contains(&s) {
            // Math.round; f64::round agrees for positive values.
            scale = ((s + f64::EPSILON) * 100.0).round() / 100.0;
        }
    }
    for (dim, limit) in [
        (&mut width, &limits.max_width),
        (&mut height, &limits.max_height),
    ] {
        if let Some(v) = dim.filter(|&v| v != 0.0) {
            let scaled = v * scale;
            if scaled > 0.0 && limit.allows(scaled) {
                *dim = Some(scaled);
            }
        }
    }
    Some(Dimension { width, height })
}

/// `extractTheme` (case-insensitive with JS toLowerCase: the Kelvin sign lowers to 'k').
pub fn extract_theme(query: &Query) -> Option<&'static str> {
    let theme = param(query, "theme")?.to_lowercase();
    ["default", "neutral", "dark", "forest"]
        .into_iter()
        .find(|t| *t == theme)
}

/// `extractPaper`, default "a4".
pub fn extract_paper(query: &Query) -> &'static str {
    const PAPERS: [&str; 11] = [
        "letter", "legal", "tabloid", "ledger", "a0", "a1", "a2", "a3", "a4", "a5", "a6",
    ];
    let paper = param(query, "paper").map(str::to_lowercase);
    PAPERS
        .into_iter()
        .find(|p| paper.as_deref() == Some(*p))
        .unwrap_or("a4")
}

/// `extractImageType`, default "jpeg".
pub fn extract_image_type(query: &Query) -> &'static str {
    let ty = query.get("type").map(str::to_lowercase);
    ["jpeg", "png", "webp"]
        .into_iter()
        .find(|t| ty.as_deref() == Some(*t))
        .unwrap_or("jpeg")
}

/// `isLandscape`: present with any value, even "false".
pub fn is_landscape(query: &Query) -> bool {
    query.get("landscape").is_some()
}

/// `isFit`: present with any value.
pub fn is_fit(query: &Query) -> bool {
    query.get("fit").is_some()
}

/// `validateQuery`; `Err` is the exact 400 message.
pub fn validate_query(query: &Query, limits: &Limits) -> Result<(), String> {
    let positive_int = |key, msg: &str| match param(query, key) {
        None => Ok(None),
        Some(s) => Some(js::parse_int(s))
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(Some)
            .ok_or_else(|| msg.to_owned()),
    };
    let width = positive_int("width", "invalid width value")?;
    let height = positive_int("height", "invalid height value")?;
    let Some(scale) = param(query, "scale") else {
        return Ok(());
    };
    if width.is_none() && height.is_none() {
        return Err("scale can only be set when either width or height is set".into());
    }
    let scale = js::parse_float(scale);
    if !scale.is_finite() || !(1.0..=3.0).contains(&scale) {
        return Err("invalid scale value - must be a number between 1 and 3".into());
    }
    for (dim, limit, name) in [
        (width, &limits.max_width, "width"),
        (height, &limits.max_height, "height"),
    ] {
        // An absent dimension is `undefined * scale` = NaN, which fails no comparison.
        let scaled = dim.unwrap_or(f64::NAN) * scale;
        if scaled <= 0.0 || limit.exceeded_by(scaled) {
            return Err(format!("the scaled {name} must be between 1 and {limit}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_like_url_search_params() {
        let q = Query::parse("?a=1+2%20%2B&fit&&b=%zz&b=second&c=%E2%82&theme=dar%E2%84%AA");
        assert_eq!(q.get("a"), Some("1 2 +"));
        assert_eq!(q.get("fit"), Some(""));
        assert_eq!(q.get("b"), Some("%zz"));
        assert_eq!(q.get("c"), Some("\u{FFFD}"));
        assert_eq!(extract_theme(&q), Some("dark"));
        assert_eq!(q.get("missing"), None);
    }

    #[test]
    fn env_limits_keep_js_semantics() {
        let q = Query::parse("width=1000&scale=1.005");
        assert_eq!(
            validate_query(&q, &Limits::from_env_values(Some("1005"), None)),
            Ok(())
        );
        // extractDimension rounds the scale to 1.01: 1010 > 1005, so the width stays unscaled.
        let d = extract_dimension(Some(&q), &Limits::from_env_values(Some("1005"), None)).unwrap();
        assert_eq!(d.width, Some(1000.0));
        assert_eq!(
            validate_query(&q, &Limits::from_env_values(Some("0"), Some(""))),
            Err("the scaled width must be between 1 and 0".into())
        );
        assert_eq!(
            validate_query(&q, &Limits::from_env_values(Some("abc"), None)),
            Ok(())
        );
        assert_eq!(
            Limits::from_env_values(Some(""), None).max_height,
            JsLimit::Number(10000.0)
        );
    }
}
