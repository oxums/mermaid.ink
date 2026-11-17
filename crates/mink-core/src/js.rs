//! The JavaScript primitives the legacy helpers leaned on: whitespace, `parseInt`, `parseFloat`,
//! `Number(string)`, `String(number)` and `JSON.parse` (V8 error texts, best effort).

use serde_json::{Map, Number, Value};

/// StrWhiteSpaceChar: what `trim`, `parseInt` and `parseFloat` skip.
pub(crate) fn is_ws(c: char) -> bool {
    c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}')
}

/// `Number.parseInt(s, 10)`.
pub(crate) fn parse_int(s: &str) -> f64 {
    let s = s.trim_start_matches(is_ws);
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = s
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    if end == 0 {
        return f64::NAN;
    }
    // Rust's parse is correctly rounded like V8 and yields inf on overflow.
    let v: f64 = s[..end].parse().unwrap_or(f64::NAN);
    if neg { -v } else { v }
}

/// `Number.parseFloat(s)`.
pub(crate) fn parse_float(s: &str) -> f64 {
    let s = s.trim_start_matches(is_ws);
    decimal_prefix(s).map_or(f64::NAN, |(v, _)| v)
}

/// `Number(s)` for a string (ToNumber), used where legacy compared numbers with env strings.
pub(crate) fn to_number(s: &str) -> f64 {
    let s = s.trim_matches(is_ws);
    if s.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = s.strip_prefix(prefix) {
            return digits
                .chars()
                .try_fold(0.0, |acc, c| {
                    Some(acc * f64::from(radix) + f64::from(c.to_digit(radix)?))
                })
                .filter(|_| !digits.is_empty())
                .unwrap_or(f64::NAN);
        }
    }
    match decimal_prefix(s) {
        Some((v, len)) if len == s.len() => v,
        _ => f64::NAN,
    }
}

/// Longest prefix of `s` that is a StrDecimalLiteral, as (value, byte length).
fn decimal_prefix(s: &str) -> Option<(f64, usize)> {
    let b = s.as_bytes();
    let digits_from = |i: usize| i + b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let start = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    if s[start..].starts_with("Infinity") {
        let inf = if b[0] == b'-' {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
        return Some((inf, start + 8));
    }
    let mut i = digits_from(start);
    let mut ndigits = i - start;
    if b.get(i) == Some(&b'.') {
        let j = digits_from(i + 1);
        if ndigits > 0 || j > i + 1 {
            ndigits += j - i - 1;
            i = j;
        }
    }
    if ndigits == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(b.get(i + 1), Some(b'+' | b'-')));
        let j = digits_from(i + 1 + sign);
        if j > i + 1 + sign {
            i = j;
        }
    }
    Some((s[..i].parse().ok()?, i))
}

/// `String(n)`.
pub(crate) fn number_to_string(n: f64) -> String {
    ryu_js::Buffer::new().format(n).to_owned()
}

/// A JS number as a JSON value: integral values become integers (merman reads config through
/// serde, which rejects `50000.0` for an integer field); non-finite ones become `null`, which is
/// what `JSON.stringify` makes of them.
fn number_value(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 && !(v == 0.0 && v.is_sign_negative())
    {
        Value::from(v as i64)
    } else {
        Number::from_f64(v).map_or(Value::Null, Value::Number)
    }
}

// V8 parses JSON iteratively; we recurse, so nesting is capped (beyond it, V8 would still parse).
// serde_json's limit: real Mermaid configs nest fewer than 6 levels, and decode/prepare must stay
// well inside a 2 MiB tokio worker stack even in debug builds (see decode.rs tests).
pub(crate) const MAX_DEPTH: usize = 128;

/// `JSON.parse(src)`. Differences from V8, all unreachable from valid diagrams: lone surrogate
/// escapes become U+FFFD (Rust strings cannot hold them), non-finite numbers become `null`, and
/// nesting deeper than [`MAX_DEPTH`] is an error. Error texts follow V8's templates.
pub(crate) fn json_parse(src: &str) -> Result<Value, String> {
    let mut p = Parser {
        src,
        b: src.as_bytes(),
        i: 0,
        depth: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i < p.b.len() {
        return Err(p.at("Unexpected non-whitespace character after JSON"));
    }
    Ok(v)
}

struct Parser<'a> {
    src: &'a str,
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    /// V8's "<msg> at position N (line L column C)", in UTF-16 units.
    fn at(&self, msg: &str) -> String {
        let units: Vec<u16> = self.src[..self.i].encode_utf16().collect();
        let (mut line, mut line_start, mut k) = (1, 0, 0);
        while k < units.len() {
            if units[k] == u16::from(b'\r') && units.get(k + 1) == Some(&u16::from(b'\n')) {
                k += 1;
            }
            if units[k] == u16::from(b'\r') || units[k] == u16::from(b'\n') {
                line += 1;
                line_start = k + 1;
            }
            k += 1;
        }
        let (pos, column) = (units.len(), units.len() - line_start + 1);
        format!("{msg} at position {pos} (line {line} column {column})")
    }

    /// V8's message for an unexpected character at the cursor.
    fn unexpected(&self) -> String {
        let Some(c) = self.src[self.i..].chars().next() else {
            return "Unexpected end of JSON input".into();
        };
        match c {
            '"' => return self.at("Unexpected string in JSON"),
            '-' | '0'..='9' => return self.at("Unexpected number in JSON"),
            _ => {}
        }
        if matches!(
            self.src,
            "[object Object]" | "undefined" | "Infinity" | "NaN"
        ) {
            return format!("\"{}\" is not valid JSON", self.src);
        }
        const CONTEXT: usize = 10;
        let units: Vec<u16> = self.src.encode_utf16().collect();
        let (pos, len) = (self.src[..self.i].encode_utf16().count(), units.len());
        let slice = |a: usize, b: usize| String::from_utf16_lossy(&units[a..b.min(len)]);
        let ctx = if len <= CONTEXT * 2 + 1 {
            format!("\"{}\"", self.src)
        } else if pos < CONTEXT {
            format!("\"{}\"...", slice(0, pos + CONTEXT))
        } else if pos < len - CONTEXT {
            format!("...\"{}\"...", slice(pos - CONTEXT, pos + CONTEXT))
        } else {
            format!("...\"{}\"", slice(pos - CONTEXT, len))
        };
        format!("Unexpected token '{c}', {ctx} is not valid JSON")
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some(b'"') => self.string().map(Value::String),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b'{') => self.nested(Self::object),
            Some(b'[') => self.nested(Self::array),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            _ => Err(self.unexpected()),
        }
    }

    fn nested(&mut self, f: fn(&mut Self) -> Result<Value, String>) -> Result<Value, String> {
        if self.depth == MAX_DEPTH {
            return Err("Maximum call stack size exceeded".into());
        }
        self.depth += 1;
        self.i += 1; // '{' or '['
        self.ws();
        let v = f(self);
        self.depth -= 1;
        v
    }

    fn object(&mut self) -> Result<Value, String> {
        let mut m = Map::new();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Object(m));
        }
        loop {
            if self.peek() != Some(b'"') {
                return Err(self.at(if m.is_empty() {
                    "Expected property name or '}' in JSON"
                } else {
                    "Expected double-quoted property name in JSON"
                }));
            }
            let k = self.string()?;
            self.ws();
            if self.peek() != Some(b':') {
                return Err(self.at("Expected ':' after property name in JSON"));
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            m.insert(k, v);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    self.ws();
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(m));
                }
                _ => return Err(self.at("Expected ',' or '}' after property value in JSON")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        let mut a = Vec::new();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Array(a));
        }
        loop {
            a.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                    self.ws();
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(a));
                }
                _ => return Err(self.at("Expected ',' or ']' after array element in JSON")),
            }
        }
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value, String> {
        for &w in word.as_bytes() {
            if self.peek() != Some(w) {
                return Err(self.unexpected());
            }
            self.i += 1;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        let digit = |p: &Self| p.peek().is_some_and(|c| c.is_ascii_digit());
        let digits = |p: &mut Self| {
            while digit(p) {
                p.i += 1;
            }
        };
        if self.peek() == Some(b'-') {
            self.i += 1;
            if !digit(self) {
                return Err(self.at("No number after minus sign in JSON"));
            }
        }
        if self.peek() == Some(b'0') {
            self.i += 1;
            if digit(self) {
                return Err(self.at("Unexpected number in JSON"));
            }
        } else {
            digits(self);
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !digit(self) {
                return Err(self.at("Unterminated fractional number in JSON"));
            }
            digits(self);
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digit(self) {
                return Err(self.at("Exponent part is missing a number in JSON"));
            }
            digits(self);
        }
        let v: f64 = self.src[start..self.i]
            .parse()
            .map_err(|_| self.unexpected())?;
        Ok(number_value(v))
    }

    fn hex4(&self, at: usize) -> Option<u16> {
        let h = self.b.get(at..at + 4)?;
        h.iter().try_fold(0u16, |acc, &c| {
            Some(acc << 4 | (c as char).to_digit(16)? as u16)
        })
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.i;
            while self
                .peek()
                .is_some_and(|c| c != b'"' && c != b'\\' && c >= 0x20)
            {
                self.i += 1;
            }
            out.push_str(&self.src[start..self.i]); // stops only at ASCII, so on a char boundary
            match self.peek() {
                None => return Err(self.at("Unterminated string in JSON")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => self.i += 1,
                Some(_) => return Err(self.at("Bad control character in string literal in JSON")),
            }
            let c = match self.peek() {
                Some(b'"') => '"',
                Some(b'\\') => '\\',
                Some(b'/') => '/',
                Some(b'b') => '\u{8}',
                Some(b'f') => '\u{c}',
                Some(b'n') => '\n',
                Some(b'r') => '\r',
                Some(b't') => '\t',
                Some(b'u') => {
                    let hi = self
                        .hex4(self.i + 1)
                        .ok_or_else(|| self.at("Bad Unicode escape in JSON"))?;
                    self.i += 4;
                    let lo = (self.b.get(self.i + 1..self.i + 3) == Some(b"\\u"))
                        .then(|| self.hex4(self.i + 3))
                        .flatten()
                        .filter(|lo| (0xDC00..0xE000).contains(lo));
                    match lo {
                        Some(lo) if (0xD800..0xDC00).contains(&hi) => {
                            self.i += 6;
                            let cp = 0x10000
                                + ((u32::from(hi) - 0xD800) << 10)
                                + (u32::from(lo) - 0xDC00);
                            char::from_u32(cp).unwrap_or('\u{FFFD}')
                        }
                        _ => char::from_u32(u32::from(hi)).unwrap_or('\u{FFFD}'), // lone surrogate
                    }
                }
                _ => return Err(self.at("Bad escaped character in JSON")),
            };
            out.push(c);
            self.i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(parse_int(" \u{feff}-12.9px"), -12.0);
        assert!(parse_int("-0").is_sign_negative());
        assert!(parse_int("x1").is_nan());
        assert_eq!(parse_int(&"9".repeat(400)), f64::INFINITY);
        assert_eq!(parse_float(" 1.5e2x"), 150.0);
        assert_eq!(parse_float("1.e"), 1.0);
        assert_eq!(parse_float("-Infinityx"), f64::NEG_INFINITY);
        assert!(parse_float(".e1").is_nan());
        assert!(parse_float("inf").is_nan());
        assert_eq!(to_number(" 0x10 "), 16.0);
        assert_eq!(to_number(""), 0.0);
        assert!(to_number("-0x10").is_nan());
        assert!(to_number("12px").is_nan());
        assert_eq!(number_to_string(-0.0), "0");
        assert_eq!(number_to_string(12.120000000000001), "12.120000000000001");
        assert_eq!(number_to_string(1e21), "1e+21");
    }

    #[test]
    fn json_values() {
        let v = json_parse(r#" {"a":[1,-0,1.5,1e400,true,null],"b":"😀\ud800\"","a":2} "#).unwrap();
        assert_eq!(v, serde_json::json!({"a": 2, "b": "😀\u{FFFD}\""}));
        let v = json_parse("[-0, 1e400, 3.0]").unwrap();
        assert!(v[0].as_f64().unwrap().is_sign_negative());
        assert_eq!(v[1], Value::Null);
        assert!(v[2].is_i64());
        let nested = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert_eq!(MAX_DEPTH, 128);
        assert!(json_parse(&nested(MAX_DEPTH)).is_ok());
        assert!(json_parse(&nested(MAX_DEPTH + 1)).is_err());
    }

    #[test]
    fn json_errors_match_v8() {
        for (src, msg) in [
            (
                "{bad json",
                "Expected property name or '}' in JSON at position 1 (line 1 column 2)",
            ),
            ("", "Unexpected end of JSON input"),
            (
                "[1",
                "Expected ',' or ']' after array element in JSON at position 2 (line 1 column 3)",
            ),
            (
                "{\"a\"\n:1,}",
                "Expected double-quoted property name in JSON at position 8 (line 2 column 4)",
            ),
            ("abc", "Unexpected token 'a', \"abc\" is not valid JSON"),
            ("undefined", "\"undefined\" is not valid JSON"),
            (
                "[1,2,3,4,5,6,7,8,9,10,x]",
                "Unexpected token 'x', ...\",7,8,9,10,x]\" is not valid JSON",
            ),
            (
                "1 2",
                "Unexpected non-whitespace character after JSON at position 2 (line 1 column 3)",
            ),
            (
                "01",
                "Unexpected number in JSON at position 1 (line 1 column 2)",
            ),
            (
                "\"a",
                "Unterminated string in JSON at position 2 (line 1 column 3)",
            ),
        ] {
            assert_eq!(json_parse(src).unwrap_err(), msg, "{src:?}");
        }
    }
}
