//! `:encodedCode` decoding: a literal port of legacy/src/helpers/getOptionsFromCode.js, plus the
//! state → (code, config) step of legacy/src/helpers/renderSVG.js.

use std::fmt;
use std::io::Read;

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use flate2::bufread::MultiGzDecoder;
use flate2::{Decompress, FlushDecompress, Status};
use serde_json::{Map, Value};

use crate::js;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// `Unknown serde type: <prefix>` (400).
    UnknownSerde(String),
    /// The inflated payload is over `max_decoded_bytes` (400). Deliberate divergence: legacy had no cap.
    TooLarge,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSerde(prefix) => write!(f, "Unknown serde type: {prefix}"),
            Self::TooLarge => f.write_str("Decoded diagram is too large"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// `getOptionsFromCode(input)`; `input` is the already percent-decoded path parameter.
/// The result is whatever `JSON.parse` produced (any JSON value, not necessarily an object).
pub fn decode(input: &str, max_decoded_bytes: usize) -> Result<Value, DecodeError> {
    if let Some(state) = deserialize_state(input, max_decoded_bytes)? {
        return Ok(state);
    }
    // Any other failure: decode the WHOLE input (prefix included) with Node's Buffer base64.
    let text = String::from_utf8_lossy(&node_buffer_base64(input)).into_owned();
    Ok(match js::json_parse(&text) {
        Ok(state) if state.get("code").is_some() => state,
        _ => Value::Object(Map::from_iter([
            ("code".to_owned(), Value::String(text)),
            (
                "mermaid".to_owned(),
                Value::String(r#"{"theme":"default"}"#.to_owned()),
            ),
        ])),
    })
}

/// `deserializeState`; `Ok(None)` is any thrown error other than UnknownSerdeError.
fn deserialize_state(state: &str, max: usize) -> Result<Option<Value>, DecodeError> {
    let Some((ty, rest)) = state.split_once(':') else {
        return Ok(from_base64(state).and_then(|t| js::json_parse(&t).ok()));
    };
    // `state.split(':')` keeps only the first two parts.
    let serialized = rest.split_once(':').map_or(rest, |(s, _)| s);
    // The prefix is checked with `in` on a plain object, so Object.prototype members count too;
    // calling them as deserializers gives these outcomes (pinned by decode.json).
    Ok(match ty {
        "pako" => match to_uint8_array(serialized) {
            Some(data) => inflate(&data, max)?.and_then(|t| js::json_parse(&t).ok()),
            None => None,
        },
        "base64" => from_base64(serialized).and_then(|t| js::json_parse(&t).ok()),
        // Object(s) is a String wrapper, which JSON.parse stringifies back.
        "constructor" => js::json_parse(serialized).ok(),
        // deserializers.hasOwnProperty(s): only the two real keys are own properties.
        "hasOwnProperty" | "propertyIsEnumerable" => {
            Some(Value::Bool(matches!(serialized, "pako" | "base64")))
        }
        "isPrototypeOf" => Some(Value::Bool(false)),
        // These return non-JSON ("[object Object]", undefined) or throw.
        "toString" | "toLocaleString" | "valueOf" | "__proto__" | "__defineGetter__"
        | "__defineSetter__" | "__lookupGetter__" | "__lookupSetter__" => None,
        _ => return Err(DecodeError::UnknownSerde(ty.to_owned())),
    })
}

/// js-base64 under the reference Node: '-'/'_' map to '+'/'/', every other non-alphabet char is
/// dropped, then `atob` (padding optional, length % 4 == 1 throws, stray low bits ignored).
const ATOB: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::RequireNone),
);

/// js-base64 `toUint8Array`; `None` = it threw.
fn to_uint8_array(s: &str) -> Option<Vec<u8>> {
    let tidy: String = s
        .chars()
        .filter_map(|c| match c {
            '-' => Some('+'),
            '_' => Some('/'),
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' => Some(c),
            _ => None,
        })
        .collect();
    ATOB.decode(tidy).ok()
}

/// js-base64 `fromBase64`: lossy UTF-8 that keeps a BOM (`ignoreBOM: true`).
fn from_base64(s: &str) -> Option<String> {
    to_uint8_array(s).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// `pako.inflate(data, {toText: true})`: zlib or gzip (by magic), lossy UTF-8 with the BOM
/// stripped (TextDecoder). `Ok(None)` = pako throws or, for a truncated stream, returns undefined.
fn inflate(data: &[u8], max: usize) -> Result<Option<String>, DecodeError> {
    let mut out = Vec::new();
    let complete = if data.starts_with(&[0x1f, 0x8b]) {
        // pako concatenates gzip members.
        let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
        MultiGzDecoder::new(data)
            .take(limit)
            .read_to_end(&mut out)
            .is_ok()
    } else {
        inflate_zlib(data, max, &mut out)
    };
    if out.len() > max {
        return Err(DecodeError::TooLarge);
    }
    if !complete {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out);
    Ok(Some(
        text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned(),
    ))
}

/// Inflates a zlib stream into `out`, stopping once `out` holds more than `max` bytes.
/// Returns false for a corrupt or truncated stream (the Read adapters silently accept truncation).
fn inflate_zlib(data: &[u8], max: usize, out: &mut Vec<u8>) -> bool {
    const CHUNK: usize = 64 * 1024;
    let mut z = Decompress::new(true);
    while out.len() <= max {
        out.reserve_exact(CHUNK.min(max + 1 - out.len()));
        let (read, written) = (z.total_in(), z.total_out());
        let input = data.get(read as usize..).unwrap_or_default();
        match z.decompress_vec(input, out, FlushDecompress::None) {
            Ok(Status::StreamEnd) => return true,
            Ok(_) if z.total_in() == read && z.total_out() == written => return false,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    true
}

/// Node `Buffer.from(s, 'base64')` (the lenient nbytes decoder): both alphabets, invalid chars
/// skipped, decoding stops at the first '='. Like Node it reads UTF-16 units truncated to a byte
/// (U+4E2D reads as '-') and sizes its output buffer the way lib/buffer.js does.
fn node_buffer_base64(s: &str) -> Vec<u8> {
    const EQ: u16 = b'=' as u16;
    let src: Vec<u16> = s.encode_utf16().collect();
    let n = src.len();
    let trimmed = |mut len: usize| {
        if len > 0 && src[len - 1] == EQ {
            len -= 1;
            if len > 0 && src[len - 1] == EQ {
                len -= 1;
            }
        }
        len
    };
    // fromStringFast: 4 bytes per char for short strings, else base64ByteLength.
    let dstlen = if n < 1024 { n * 4 } else { trimmed(n) * 3 / 4 };
    let decoded_size = match trimmed(n) {
        _ if n < 2 => 0,
        size if size > 1 => size / 4 * 3 + (size % 4).div_ceil(2),
        _ => 0,
    };
    let max_k = dstlen.min(decoded_size) / 3 * 3;
    let mut max_i = n / 4 * 4;
    let mut dst = Vec::with_capacity(dstlen.min(decoded_size) + 3);
    let mut i = 0;
    while i < max_i && dst.len() < max_k {
        let t = [
            unbase64(src[i]),
            unbase64(src[i + 1]),
            unbase64(src[i + 2]),
            unbase64(src[i + 3]),
        ];
        if t.iter().any(|&v| v > 63) {
            if !decode_group_slow(&src, dstlen, &mut i, &mut dst) {
                return dst;
            }
            max_i = i + (n - i) / 4 * 4;
        } else {
            dst.extend([
                t[0] << 2 | t[1] >> 4,
                (t[1] & 0x0f) << 4 | t[2] >> 2,
                (t[2] & 0x03) << 6 | t[3],
            ]);
            i += 4;
        }
    }
    if i < n && dst.len() < dstlen {
        decode_group_slow(&src, dstlen, &mut i, &mut dst);
    }
    dst
}

fn unbase64(unit: u16) -> u8 {
    match unit as u8 {
        c @ b'A'..=b'Z' => c - b'A',
        c @ b'a'..=b'z' => c - b'a' + 26,
        c @ b'0'..=b'9' => c - b'0' + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        _ => 255,
    }
}

/// nbytes `Base64DecodeGroupSlow`: one 4-char group, skipping invalid chars; false = stop.
fn decode_group_slow(src: &[u16], dstlen: usize, i: &mut usize, dst: &mut Vec<u8>) -> bool {
    let mut hi = 0;
    for step in 0..4 {
        let lo = loop {
            let unit = src[*i];
            *i += 1;
            let v = unbase64(unit);
            if v < 64 {
                break v;
            }
            if unit as u8 == b'=' || *i >= src.len() {
                return false;
            }
        };
        match step {
            1 => dst.push((hi & 0x3f) << 2 | (lo & 0x30) >> 4),
            2 => dst.push((hi & 0x0f) << 4 | (lo & 0x3c) >> 2),
            3 => dst.push((hi & 0x03) << 6 | lo),
            _ => {}
        }
        if *i >= src.len() || dst.len() >= dstlen {
            return false;
        }
        hi = lo;
    }
    true
}

/// renderSVG.js before `window.App.render`: `{code, mermaid: config} = state`, a string config is
/// JSON.parsed, and `?theme` is assigned onto it. Returns the code and the object mermaid.initialize
/// receives before `startOnLoad`/`securityLevel` are forced (`{...config}`: always an object).
/// `Err` is the 400 body; texts for non-SyntaxError failures mimic V8's TypeErrors (best effort).
pub fn prepare(state: Value, theme: Option<&str>) -> Result<(String, Value), String> {
    let (code, config) = match state {
        Value::Null => {
            return Err(
                "Cannot destructure property 'code' of 'getOptionsFromCode(...)' as it is null."
                    .into(),
            );
        }
        Value::Object(mut m) => (m.remove("code"), m.remove("mermaid")),
        _ => (None, None), // primitives and arrays have no `code`/`mermaid`
    };
    let config = match config {
        Some(Value::String(s)) => Some(js::json_parse(&s)?),
        other => other,
    };
    let mut config = match config {
        Some(Value::Object(m)) => m,
        // Spreading an array copies its indices, which mermaid ignores; theme is still assigned.
        Some(Value::Array(_)) => Map::new(),
        // Spreading undefined/null/primitives gives {} (a string's indices are ignored too).
        _ if theme.is_none() => Map::new(),
        None => return Err("Cannot set properties of undefined (setting 'theme')".into()),
        Some(Value::Null) => return Err("Cannot set properties of null (setting 'theme')".into()),
        Some(primitive) => {
            let (ty, shown) = match primitive {
                Value::String(s) => ("string", s),
                Value::Number(n) => (
                    "number",
                    n.as_f64()
                        .map_or_else(|| n.to_string(), js::number_to_string),
                ),
                other => ("boolean", other.to_string()),
            };
            return Err(format!("Cannot create property 'theme' on {ty} '{shown}'"));
        }
    };
    if let Some(theme) = theme {
        config.insert("theme".into(), Value::String(theme.into()));
    }
    let code = match code {
        Some(Value::String(code)) => code,
        None => return Err("Cannot read properties of undefined (reading 'replace')".into()),
        Some(Value::Null) => {
            return Err("Cannot read properties of null (reading 'replace')".into());
        }
        // shortcut: mermaid's exact TypeError for a non-string definition was not captured; only the 400 matters.
        Some(_) => return Err("text.replace is not a function".into()),
    };
    Ok((code, Value::Object(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn pako(data: &[u8]) -> String {
        format!(
            "pako:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
        )
    }

    #[test]
    fn inflate_cap_is_a_distinct_error() {
        let doc = format!(r#"{{"code":"{}"}}"#, "a".repeat(10_000));
        assert!(decode(&pako(&zlib(doc.as_bytes())), doc.len()).is_ok());
        assert_eq!(
            decode(&pako(&zlib(doc.as_bytes())), doc.len() - 1),
            Err(DecodeError::TooLarge)
        );
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(doc.as_bytes()).unwrap();
        assert_eq!(
            decode(&pako(&gz.finish().unwrap()), 100),
            Err(DecodeError::TooLarge)
        );
    }

    #[test]
    fn truncated_or_corrupt_streams_fall_back() {
        let z = zlib(br#"{"code":"a"}"#);
        // Without the adler32 trailer pako never reaches the stream end: fallback, not success.
        for bad in [&z[..z.len() - 4], &z[..z.len() - 1]] {
            let v = decode(&pako(bad), 1 << 20).unwrap();
            assert_eq!(v["mermaid"], r#"{"theme":"default"}"#, "{bad:?}");
        }
        let mut corrupt = z.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(
            decode(&pako(&corrupt), 1 << 20).unwrap()["mermaid"],
            r#"{"theme":"default"}"#
        );
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(br#"{"code":"a"}"#).unwrap();
        let gz = gz.finish().unwrap();
        let v = decode(&pako(&gz[..gz.len() - 8]), 1 << 20).unwrap();
        assert_eq!(
            v["mermaid"], r#"{"theme":"default"}"#,
            "gzip without its trailer"
        );
    }

    #[test]
    fn node_buffer_quirks() {
        assert_eq!(node_buffer_base64("ab==cd"), b"i");
        assert_eq!(node_buffer_base64("!!eyJ j"), node_buffer_base64("eyJj"));
        assert_eq!(node_buffer_base64("a"), b"");
        assert_eq!(node_buffer_base64("YQ\u{4e2d}"), node_buffer_base64("YQ-"));
    }

    #[test]
    fn prepare_follows_render_svg() {
        let st = |v: Value| prepare(v, None);
        assert_eq!(
            st(json!({"code": "a", "mermaid": "{\"theme\":\"dark\"}"})),
            Ok(("a".into(), json!({"theme": "dark"})))
        );
        assert_eq!(
            prepare(json!({"code": "a", "mermaid": {"x": 1}}), Some("forest")),
            Ok(("a".into(), json!({"x": 1, "theme": "forest"})))
        );
        assert_eq!(
            st(json!({"code": "a", "mermaid": null})),
            Ok(("a".into(), json!({})))
        );
        assert_eq!(
            prepare(json!({"code": "a", "mermaid": []}), Some("dark")),
            Ok(("a".into(), json!({"theme": "dark"})))
        );
        assert_eq!(
            prepare(json!({"code": "a", "mermaid": null}), Some("dark")).unwrap_err(),
            "Cannot set properties of null (setting 'theme')"
        );
        assert_eq!(
            prepare(json!({"code": "a", "mermaid": 42}), Some("dark")).unwrap_err(),
            "Cannot create property 'theme' on number '42'"
        );
        assert_eq!(
            prepare(json!({"code": "a"}), Some("dark")).unwrap_err(),
            "Cannot set properties of undefined (setting 'theme')"
        );
        assert_eq!(
            st(json!({"code": "a", "mermaid": "{bad json"})).unwrap_err(),
            "Expected property name or '}' in JSON at position 1 (line 1 column 2)"
        );
        assert_eq!(
            st(json!(false)).unwrap_err(),
            "Cannot read properties of undefined (reading 'replace')"
        );
        assert!(
            st(Value::Null)
                .unwrap_err()
                .starts_with("Cannot destructure property 'code'")
        );
        assert!(st(json!({"code": 1})).is_err());
    }

    /// decode + prepare at the maximum nesting, in a debug build, on a tokio-worker-sized stack.
    #[test]
    fn max_depth_fits_a_2_mib_stack() {
        let deep = |n: usize| format!("{}1{}", r#"{"a":"#.repeat(n), "}".repeat(n));
        let config = deep(js::MAX_DEPTH);
        // The state itself nests to the limit as well: {"code","mermaid","x":{...}}.
        let state = format!(
            r#"{{"code":"graph TD","mermaid":{},"x":{}}}"#,
            serde_json::to_string(&config).unwrap(),
            deep(js::MAX_DEPTH - 1)
        );
        let input = pako(&zlib(state.as_bytes()));
        let worker = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                let state = decode(&input, 1 << 20).unwrap();
                assert!(state["x"].is_object(), "state parsed, not the fallback");
                let (code, config) = prepare(state, Some("dark")).unwrap();
                assert_eq!(code, "graph TD");
                assert_eq!(config["theme"], "dark");
                // One level deeper is refused, not a stack overflow.
                let too_deep = json!({"code": "a", "mermaid": deep(js::MAX_DEPTH + 1)});
                assert!(prepare(too_deep, None).is_err());
            })
            .unwrap();
        worker.join().unwrap();
    }
}
