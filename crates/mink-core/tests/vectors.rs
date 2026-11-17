//! Every entry of tests/fixtures/vectors/*.json (outputs of the legacy JS helpers) must match.
//! Entries with a repeated query param (an array `in`) check our documented first-value rule
//! instead: the repeated query must behave exactly like the query holding only the first values
//! (and like legacy, where legacy did not throw).

use std::path::PathBuf;

use mink_core::decode::{DecodeError, decode};
use mink_core::key::{create_cache_key, get_query_key};
use mink_core::query::{
    JsLimit, Limits, Query, extract_bg_color, extract_dimension, extract_image_type, extract_paper,
    extract_theme, is_fit, is_landscape, validate_query,
};
use serde_json::{Map, Value, json};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/vectors")
}

/// Loads a vector file. JSON.stringify writes lone surrogates as `\udXXX` escapes, which serde_json
/// rejects; they are rewritten to `�` (what our decoder produces for them) and counted.
fn load(name: &str) -> (Value, usize) {
    let text = std::fs::read_to_string(dir().join(name)).unwrap();
    let (mut out, mut replaced) = (String::with_capacity(text.len()), 0);
    let mut rest = text.as_str();
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let unit = |s: &str| {
            s.get(2..6)
                .filter(|_| s.starts_with("\\u"))
                .and_then(|h| u16::from_str_radix(h, 16).ok())
        };
        match unit(rest) {
            Some(0xD800..=0xDBFF)
                if unit(&rest[6..]).is_some_and(|lo| (0xDC00..0xE000).contains(&lo)) =>
            {
                out.push_str(&rest[..12]);
                rest = &rest[12..];
            }
            Some(0xD800..=0xDFFF) => {
                out.push_str("\\ufffd");
                replaced += 1;
                rest = &rest[6..];
            }
            _ => {
                out.push_str(&rest[..2]);
                rest = &rest[2..];
            }
        }
    }
    out.push_str(rest);
    (serde_json::from_str(&out).unwrap(), replaced)
}

fn entries(v: &Value, key: &str) -> Vec<Value> {
    v[key].as_array().unwrap().clone()
}

/// Percent-encodes everything but alphanumerics, so the real query parser is exercised.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// (query with repeated keys, query with only the first values, had an array value).
fn queries(obj: &Map<String, Value>) -> (Query, Query, bool) {
    let (mut all, mut firsts, mut array) = (Vec::new(), Vec::new(), false);
    for (k, v) in obj {
        match v {
            Value::String(s) => {
                all.push(format!("{}={}", enc(k), enc(s)));
                firsts.push(format!("{}={}", enc(k), enc(s)));
            }
            Value::Array(items) => {
                array = true;
                for (i, s) in items.iter().enumerate() {
                    let pair = format!("{}={}", enc(k), enc(s.as_str().unwrap()));
                    if i == 0 {
                        firsts.push(pair.clone());
                    }
                    all.push(pair);
                }
            }
            other => panic!("unexpected query value {other}"),
        }
    }
    (
        Query::parse(&all.join("&")),
        Query::parse(&firsts.join("&")),
        array,
    )
}

fn limits(v: Option<&Value>) -> Limits {
    let limit = |k: &str| match v.and_then(|o| o.get(k)) {
        None => JsLimit::Undefined,
        Some(Value::Number(n)) => JsLimit::Number(n.as_f64().unwrap()),
        Some(Value::String(s)) => JsLimit::String(s.clone()),
        Some(other) => panic!("unexpected limit {other}"),
    };
    Limits {
        max_width: limit("maxWidth"),
        max_height: limit("maxHeight"),
    }
}

/// Equality of JS values as JSON: numbers compare numerically (-0 == 0, 1 == 1.0).
fn js_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| js_eq(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| js_eq(v, w)))
        }
        _ => a == b,
    }
}

#[derive(Default)]
struct Tally {
    passed: usize,
    arrays: usize,
    failures: Vec<String>,
}

impl Tally {
    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        if ok {
            self.passed += 1;
        } else {
            self.failures.push(what());
        }
    }

    fn finish(self, file: &str, total: usize, extra: &str) {
        eprintln!(
            "{file}: {}/{total} entries pass ({} via the first-value rule){extra}",
            self.passed, self.arrays
        );
        for f in self.failures.iter().take(25) {
            eprintln!("  FAIL {f}");
        }
        assert!(
            self.failures.is_empty(),
            "{file}: {} failures",
            self.failures.len()
        );
        assert_eq!(self.passed, total, "{file}: every entry must be checked");
    }
}

fn type_tag(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[test]
fn decode_vectors() {
    let (file, replaced) = load("decode.json");
    let rows = entries(&file, "entries");
    let mut t = Tally::default();
    let mut skipped = Vec::new();
    for e in &rows {
        let input = e["input"].as_str().unwrap();
        // A lone surrogate in the input (read as U+FFFD by `load`) cannot reach the server: Rust
        // strings cannot hold one, and legacy's decodeURIComponent threw (500) before decoding.
        if input.contains('\u{FFFD}') {
            skipped.push(input);
            continue;
        }
        let got = decode(input, 1 << 20);
        let ok = match (e["outcome"].as_str().unwrap(), &got) {
            ("ok", Ok(v)) => {
                e["value_type"] == type_tag(v) && e.get("value").is_some_and(|want| js_eq(v, want))
            }
            ("unknown_serde", Err(err @ DecodeError::UnknownSerde(_))) => {
                e["error_message"] == err.to_string()
            }
            _ => false,
        };
        t.check(ok, || format!("input {input:?}: want {e}, got {got:?}"));
    }
    let note = format!(
        ", {replaced} lone-surrogate escapes read as U+FFFD, {} skipped (lone surrogate in input): {skipped:?}",
        skipped.len()
    );
    assert!(
        skipped.len() <= 1,
        "only one fuzz input is known to hold a lone surrogate"
    );
    t.finish("decode.json", rows.len() - skipped.len(), &note);
}

#[test]
fn query_vectors() {
    let (file, _) = load("query.json");
    let rows = entries(&file, "rows");
    let tokens = |k: &str| -> Vec<String> {
        file[k]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    let (widths, heights, scales) = (
        tokens("width_tokens"),
        tokens("height_tokens"),
        tokens("scale_tokens"),
    );
    let states: Vec<Limits> = file["states"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| limits(s.as_object().map(|_| s)))
        .collect();
    let messages = file["messages"].as_array().unwrap();
    let numbers: Vec<u64> = file["numbers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| u64::from_str_radix(n["bits"].as_str().unwrap(), 16).unwrap())
        .collect();
    let mut t = Tally::default();
    for row in &rows {
        let r: Vec<i64> = row
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap_or(-1))
            .collect();
        let pairs = [
            ("width", &widths[r[0] as usize]),
            ("height", &heights[r[1] as usize]),
            ("scale", &scales[r[2] as usize]),
        ];
        let raw: Vec<String> = pairs
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k}={}", enc(v)))
            .collect();
        let q = Query::parse(&raw.join("&"));
        let lim = &states[r[3] as usize];
        let want_v = (r[4] >= 0).then(|| messages[r[4] as usize].as_str().unwrap().to_owned());
        let want_dim = |i: usize| row[i].as_u64().map(|n| numbers[n as usize]);
        let got_v = validate_query(&q, lim).err();
        let d = extract_dimension(Some(&q), lim).unwrap();
        let got_dim = (d.width.map(f64::to_bits), d.height.map(f64::to_bits));
        t.check(
            got_v == want_v && got_dim == (want_dim(5), want_dim(6)),
            || {
                format!(
                    "{pairs:?} {lim:?}: want {want_v:?} {:?}, got {got_v:?} {d:?}",
                    (want_dim(5), want_dim(6))
                )
            },
        );
    }
    t.finish("query.json", rows.len(), "");
}

/// bg_color/theme/paper/image_type/flags: `{in?, out|error}` per entry.
fn single_param(name: &str, eval: impl Fn(&str, &Query) -> Value) {
    let (file, _) = load(name);
    let rows = entries(&file, "entries");
    let mut t = Tally::default();
    for e in &rows {
        let f = e.get("fn").and_then(Value::as_str).unwrap_or("");
        let key = file.get("key").and_then(Value::as_str).unwrap_or(match f {
            "isLandscape" => "landscape",
            _ => "fit",
        });
        let mut obj = Map::new();
        if let Some(v) = e.get("in") {
            obj.insert(key.to_owned(), v.clone());
        }
        // A query of `undefined` behaves like an empty query for all of these.
        let (all, firsts, array) = queries(&obj);
        let got = eval(f, &all);
        // `[]` cannot come from a query string (a repeated key has 2+ values): no legacy comparison.
        let producible = e
            .get("in")
            .is_none_or(|v| v.as_array().is_none_or(|a| !a.is_empty()));
        let legacy_ok = !producible || e.get("out").is_none_or(|want| *want == got);
        if array {
            t.arrays += 1;
            t.check(got == eval(f, &firsts) && legacy_ok, || {
                format!("{e}: got {got}")
            });
        } else {
            t.check(e.get("out").is_some() && legacy_ok, || {
                format!("{e}: got {got}")
            });
        }
    }
    t.finish(name, rows.len(), "");
}

#[test]
fn bg_color_vectors() {
    single_param("bg_color.json", |_, q| json!(extract_bg_color(q)));
}

#[test]
fn theme_vectors() {
    single_param("theme.json", |_, q| json!(extract_theme(q)));
}

#[test]
fn paper_vectors() {
    single_param("paper.json", |_, q| json!(extract_paper(q)));
}

#[test]
fn image_type_vectors() {
    single_param("image_type.json", |_, q| json!(extract_image_type(q)));
}

#[test]
fn flags_vectors() {
    single_param("flags.json", |f, q| {
        json!(if f == "isLandscape" {
            is_landscape(q)
        } else {
            is_fit(q)
        })
    });
}

/// query_key/cache_key: `query` (absent = undefined) and `options` objects, `{out|error}`.
fn keyed(
    name: &str,
    args_of: fn(&Value) -> &Value,
    eval: impl Fn(&Value, Option<&Query>, &Limits) -> String,
) {
    let (file, _) = load(name);
    let rows = entries(&file, "entries");
    let mut t = Tally::default();
    for e in &rows {
        let args = args_of(e);
        let lim = limits(args.get("options"));
        let got_for = |q: Option<&Query>| eval(args, q, &lim);
        let Some(obj) = args.get("query").map(|q| q.as_object().unwrap()) else {
            t.check(e["out"] == got_for(None), || {
                format!("{e}: got {}", got_for(None))
            });
            continue;
        };
        let (all, firsts, array) = queries(obj);
        let got = got_for(Some(&all));
        let legacy_ok = e.get("out").is_none_or(|want| *want == got);
        if array {
            t.arrays += 1;
            t.check(got == got_for(Some(&firsts)) && legacy_ok, || {
                format!("{e}: got {got}")
            });
        } else {
            t.check(e.get("out").is_some() && legacy_ok, || {
                format!("{e}: got {got}")
            });
        }
    }
    t.finish(name, rows.len(), "");
}

#[test]
fn query_key_vectors() {
    keyed(
        "query_key.json",
        |e| e,
        |a, q, lim| get_query_key(a["assetType"].as_str().unwrap(), q, lim),
    );
}

#[test]
fn cache_key_vectors() {
    keyed(
        "cache_key.json",
        |e| &e["args"],
        |a, q, lim| {
            let s =
                |k: &str, default: &'static str| a.get(k).map_or(default, |v| v.as_str().unwrap());
            create_cache_key(s("assetType", "img"), s("encodedCode", ""), q, lim)
        },
    );
}

#[test]
fn every_vector_file_is_covered() {
    let covered = [
        "bg_color.json",
        "cache_key.json",
        "decode.json",
        "flags.json",
        "image_type.json",
        "meta.json",
        "paper.json",
        "query.json",
        "query_key.json",
        "theme.json",
    ];
    for f in std::fs::read_dir(dir()).unwrap() {
        let name = f.unwrap().file_name().into_string().unwrap();
        assert!(
            covered.contains(&name.as_str()),
            "{name} has no vector test"
        );
    }
    let (meta, _) = load("meta.json");
    assert_eq!(
        meta["js_base64"], "3.9.6",
        "vectors were generated with the js-base64 this port mimics"
    );
}
