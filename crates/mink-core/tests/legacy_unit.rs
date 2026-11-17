//! 1:1 ports of legacy/test/utils.test.js and legacy/test/createCacheKey.test.js.
//! `test.each` tables become loops; JS `{maxWidth: 150}` options become number limits.

use mink_core::key::{create_cache_key, get_query_key};
use mink_core::query::{Dimension, JsLimit, Limits, Query, extract_dimension, validate_query};

fn q(pairs: &[(&str, &str)]) -> Query {
    pairs.iter().copied().collect()
}

fn lim(max_width: Option<f64>, max_height: Option<f64>) -> Limits {
    let l = |v: Option<f64>| v.map_or(JsLimit::Undefined, JsLimit::Number);
    Limits {
        max_width: l(max_width),
        max_height: l(max_height),
    }
}

fn none() -> Limits {
    Limits::default()
}

fn validate(pairs: &[(&str, &str)]) -> Result<(), String> {
    validate_query(&q(pairs), &none())
}

fn dim(pairs: &[(&str, &str)], limits: &Limits) -> Option<Dimension> {
    extract_dimension(Some(&q(pairs)), limits)
}

fn wh(width: Option<f64>, height: Option<f64>) -> Option<Dimension> {
    Some(Dimension { width, height })
}

const INVALID_SCALE: &str = "invalid scale value - must be a number between 1 and 3";

// ---------------- validateQuery ----------------

#[test]
fn validate_works_without_any_arguments() {
    assert_eq!(validate_query(&Query::default(), &none()), Ok(()));
}

#[test]
fn validate_width() {
    for w in ["1", "999999999"] {
        assert_eq!(validate(&[("width", w)]), Ok(()), "{w}");
    }
    for w in ["nan", "0", "-1"] {
        assert_eq!(
            validate(&[("width", w)]),
            Err("invalid width value".into()),
            "{w}"
        );
    }
}

#[test]
fn validate_height() {
    for h in ["1", "999999999"] {
        assert_eq!(validate(&[("height", h)]), Ok(()), "{h}");
    }
    for h in ["nan", "0", "-1"] {
        assert_eq!(
            validate(&[("height", h)]),
            Err("invalid height value".into()),
            "{h}"
        );
    }
}

#[test]
fn validate_accepts_valid_scales() {
    for s in [
        "1", "1.0", "1.00", "1.5", "2.0", "2.25", "2.5", "2.75", "3", "3.0", "3.00",
    ] {
        assert_eq!(
            validate(&[("scale", s), ("width", "100"), ("height", "100")]),
            Ok(()),
            "{s}"
        );
    }
    for s in ["1", "1.0", "2.5", "3"] {
        assert_eq!(
            validate(&[("scale", s), ("width", "100")]),
            Ok(()),
            "only width {s}"
        );
        assert_eq!(
            validate(&[("scale", s), ("height", "100")]),
            Ok(()),
            "only height {s}"
        );
    }
}

#[test]
fn validate_unrounded_scale_values() {
    for s in [
        "1.234", "1.235", "1.23456", "1.23567", "2.994", "2.995", "2.999", "1.004", "1.005",
        "1.001", "3.0", "3.00", "1.0", "1.00",
    ] {
        assert_eq!(
            validate(&[("scale", s), ("width", "100"), ("height", "100")]),
            Ok(()),
            "{s}"
        );
    }
    for s in ["3.001", "3.005", "0.999"] {
        assert_eq!(
            validate(&[("scale", s), ("width", "100"), ("height", "100")]),
            Err(INVALID_SCALE.into()),
            "{s}"
        );
    }
}

#[test]
fn validate_scale_string_format_variations() {
    for s in [" 1.5 ", "001.50", "1.5000", "2.000"] {
        assert_eq!(
            validate(&[("scale", s), ("width", "100"), ("height", "100")]),
            Ok(()),
            "{s:?}"
        );
    }
}

#[test]
fn validate_rejects_invalid_scales() {
    for s in [
        "0",
        "0.9",
        "0.99",
        "3.01",
        "3.1",
        "4",
        "-1",
        "-0.5",
        "nan",
        "NaN",
        "infinity",
        "-infinity",
        "abc",
        " ",
        "null",
        "undefined",
        "1e10",
        "1e-10",
    ] {
        assert_eq!(
            validate(&[("scale", s), ("width", "100")]),
            Err(INVALID_SCALE.into()),
            "{s:?}"
        );
    }
}

#[test]
fn validate_scale_without_required_dimensions() {
    assert_eq!(
        validate(&[("scale", "1.5")]),
        Err("scale can only be set when either width or height is set".into())
    );
    assert_eq!(
        validate(&[
            ("scale", "1.5"),
            ("width", "invalid"),
            ("height", "invalid")
        ]),
        Err("invalid width value".into())
    );
}

#[test]
fn validate_width_limits() {
    let v = |pairs: &[(&str, &str)], l| validate_query(&q(pairs), &l);
    assert_eq!(
        v(&[("scale", "2"), ("width", "100")], lim(Some(150.0), None)),
        Err("the scaled width must be between 1 and 150".into())
    );
    assert_eq!(
        v(&[("scale", "2"), ("width", "0")], lim(Some(100.0), None)),
        Err("invalid width value".into())
    );
    assert_eq!(
        v(
            &[("scale", "1.5"), ("width", "100")],
            lim(Some(150.0), None)
        ),
        Ok(())
    );
    assert_eq!(v(&[("scale", "3"), ("width", "10000")], none()), Ok(()));
}

#[test]
fn validate_height_limits() {
    let v = |pairs: &[(&str, &str)], l| validate_query(&q(pairs), &l);
    assert_eq!(
        v(&[("scale", "2"), ("height", "100")], lim(None, Some(150.0))),
        Err("the scaled height must be between 1 and 150".into())
    );
    assert_eq!(
        v(&[("scale", "2"), ("height", "0")], lim(None, Some(100.0))),
        Err("invalid height value".into())
    );
    assert_eq!(
        v(
            &[("scale", "1.5"), ("height", "100")],
            lim(None, Some(150.0))
        ),
        Ok(())
    );
    assert_eq!(v(&[("scale", "3"), ("height", "10000")], none()), Ok(()));
}

#[test]
fn validate_both_dimensions_with_limits() {
    let v = |pairs: &[(&str, &str)], l| validate_query(&q(pairs), &l);
    assert_eq!(
        v(
            &[("scale", "2"), ("width", "100"), ("height", "50")],
            lim(Some(150.0), Some(200.0))
        ),
        Err("the scaled width must be between 1 and 150".into())
    );
    assert_eq!(
        v(
            &[("scale", "2"), ("width", "50"), ("height", "100")],
            lim(Some(200.0), Some(150.0))
        ),
        Err("the scaled height must be between 1 and 150".into())
    );
    assert_eq!(
        v(
            &[("scale", "2"), ("width", "50"), ("height", "50")],
            lim(Some(200.0), Some(200.0))
        ),
        Ok(())
    );
    assert_eq!(
        v(
            &[("scale", "2"), ("width", "50"), ("height", "100")],
            lim(Some(200.0), None)
        ),
        Ok(())
    );
    assert_eq!(
        v(
            &[("scale", "2"), ("width", "100"), ("height", "50")],
            lim(None, Some(200.0))
        ),
        Ok(())
    );
}

#[test]
fn validate_edge_cases_and_special_scenarios() {
    for s in [
        "1.123456789012345",
        "001.500",
        "1.0000001",
        "1.0049",
        "2.9949",
    ] {
        assert_eq!(validate(&[("scale", s), ("width", "100")]), Ok(()), "{s}");
    }
}

#[test]
fn validate_multiple_invalid_dimensions() {
    assert_eq!(
        validate(&[("width", "-1"), ("height", "-1")]),
        Err("invalid width value".into())
    );
    assert_eq!(
        validate(&[("width", "-1"), ("scale", "0.5")]),
        Err("invalid width value".into())
    );
    assert_eq!(
        validate(&[("width", "100"), ("height", "-1")]),
        Err("invalid height value".into())
    );
}

#[test]
fn validate_boundary_values() {
    assert_eq!(validate(&[("width", "1")]), Ok(()));
    assert_eq!(validate(&[("height", "1")]), Ok(()));
    assert_eq!(validate(&[("scale", "1"), ("width", "100")]), Ok(()));
    assert_eq!(validate(&[("scale", "3"), ("width", "100")]), Ok(()));
    assert_eq!(
        validate(&[("width", "0")]),
        Err("invalid width value".into())
    );
    assert_eq!(
        validate(&[("height", "0")]),
        Err("invalid height value".into())
    );
}

// ---------------- extractDimension ----------------

#[test]
fn extract_dimension_basic_functionality() {
    // null and undefined are both "no query"
    assert_eq!(extract_dimension(None, &none()), None);
    assert_eq!(dim(&[], &none()), wh(None, None));
    assert_eq!(dim(&[("width", "800")], &none()), wh(Some(800.0), None));
    assert_eq!(dim(&[("height", "600")], &none()), wh(None, Some(600.0)));
    assert_eq!(
        dim(&[("width", "800"), ("height", "600")], &none()),
        wh(Some(800.0), Some(600.0))
    );
    assert_eq!(dim(&[("width", "invalid")], &none()), wh(None, None));
    assert_eq!(dim(&[("height", "invalid")], &none()), wh(None, None));
}

#[test]
fn extract_dimension_scale_rounding_to_2_decimal_places() {
    for (scale, expected) in [
        ("1.234", 123.0),
        ("1.235", 124.0),
        ("1.23456", 123.0),
        ("1.23567", 124.0),
    ] {
        let d = dim(
            &[("width", "100"), ("height", "100"), ("scale", scale)],
            &none(),
        );
        assert_eq!(d, wh(Some(expected), Some(expected)), "{scale}");
    }
    for (scale, expected) in [
        ("2.994", 299.0),
        ("2.995", 300.0),
        ("1.004", 100.0),
        ("1.005", 101.0),
    ] {
        assert_eq!(
            dim(&[("width", "100"), ("scale", scale)], &none()),
            wh(Some(expected), None),
            "{scale}"
        );
    }
    for (scale, expected) in [("1.0", 100.0), ("2.0", 200.0), ("3.0", 300.0), ("2", 200.0)] {
        let d = dim(
            &[("width", "100"), ("height", "100"), ("scale", scale)],
            &none(),
        );
        assert_eq!(d, wh(Some(expected), Some(expected)), "{scale}");
    }
}

#[test]
fn extract_dimension_scale_precision_with_epsilon() {
    assert_eq!(
        dim(&[("width", "100"), ("scale", "1.555")], &none()),
        wh(Some(156.0), None)
    );
    let d = dim(&[("width", "100"), ("scale", "2.445")], &none()).unwrap();
    assert!((d.width.unwrap() - 245.0).abs() < 5e-11); // toBeCloseTo(245, 10)
    assert_eq!(d.height, None);
    assert_eq!(
        dim(&[("width", "1000"), ("scale", "1.0001")], &none()),
        wh(Some(1000.0), None)
    );
    assert_eq!(
        dim(&[("width", "1000"), ("scale", "1.0049")], &none()),
        wh(Some(1000.0), None)
    );
    assert_eq!(
        dim(&[("width", "1000"), ("scale", "1.0050")], &none()),
        wh(Some(1010.0), None)
    );
}

#[test]
fn extract_dimension_scale_validation_boundaries() {
    for (scale, expected) in [
        ("1", 100.0),
        ("3", 300.0),
        ("0.9", 100.0),
        ("3.01", 100.0),
        ("invalid", 100.0),
    ] {
        assert_eq!(
            dim(&[("width", "100"), ("scale", scale)], &none()),
            wh(Some(expected), None),
            "{scale}"
        );
    }
    assert_eq!(dim(&[("scale", "2")], &none()), wh(None, None));
}

#[test]
fn extract_dimension_scale_with_max_constraints() {
    assert_eq!(
        dim(&[("width", "100"), ("scale", "2")], &lim(Some(150.0), None)),
        wh(Some(100.0), None)
    );
    assert_eq!(
        dim(
            &[("height", "100"), ("scale", "2")],
            &lim(None, Some(150.0))
        ),
        wh(None, Some(100.0))
    );
    let both = |w, h, s, l| dim(&[("width", w), ("height", h), ("scale", s)], &l);
    assert_eq!(
        both("100", "100", "1.5", lim(Some(200.0), Some(200.0))),
        wh(Some(150.0), Some(150.0))
    );
    assert_eq!(
        both("100", "100", "2", lim(Some(200.0), Some(200.0))),
        wh(Some(200.0), Some(200.0))
    );
    assert_eq!(
        both("100", "50", "2", lim(Some(150.0), Some(200.0))),
        wh(Some(100.0), Some(100.0))
    );
    assert_eq!(
        both("50", "100", "2", lim(Some(200.0), Some(150.0))),
        wh(Some(100.0), Some(100.0))
    );
    assert_eq!(
        both("100", "100", "3", lim(Some(200.0), Some(200.0))),
        wh(Some(100.0), Some(100.0))
    );
}

#[test]
fn extract_dimension_scale_rounding_with_constraints() {
    assert_eq!(
        dim(
            &[("width", "100"), ("scale", "1.995")],
            &lim(Some(199.0), None)
        ),
        wh(Some(100.0), None)
    );
    assert_eq!(
        dim(
            &[("height", "100"), ("scale", "1.995")],
            &lim(None, Some(199.0))
        ),
        wh(None, Some(100.0))
    );
    assert_eq!(
        dim(
            &[("width", "100"), ("scale", "1.994")],
            &lim(Some(199.0), None)
        ),
        wh(Some(199.0), None)
    );
}

// ---------------- createCacheKey ----------------

const CODE: &str = "pako:eNpVkM1qw0AMhF9F6NRC_AI-BBo7zSWQQHLz5iC8SnZJ9gd5TQm2373r_ECrk9B8MwwasA2ascSLUDRwrJWHPF9NZcR2yVF3gqJYjhtO4ILn-wirj02AzoQYrb98PvnVDEE1bGeMIRnrr9NTqh7-necR6mZLMYV4-qscf8II68buTY7_rxjh7PpuzlSeqWhJoCJ5IbhAx-LI6lx-mG8Kk2HHCsu8apKrQuWnzPVRU-K1tikI5qRbxwukPoXD3bdYJun5DdWW8iPci5p-AUT3W9o";

#[test]
fn cache_key_works() {
    assert_eq!(
        create_cache_key("img", CODE, None, &none()),
        "bf1124a1afd44220d64ca9fbe5a25ecdc6aa233362a1f85f7ed19b16902de95e"
    );
}

#[test]
fn cache_key_works_with_query() {
    assert_eq!(
        create_cache_key("img", CODE, Some(&q(&[("type", "png")])), &none()),
        "4ed5db750c700436186b8bf2df5110a372f6c19c79da49e987b5dd71b5a65d4d"
    );
}

#[test]
fn cache_key_creates_32_bytes_result() {
    assert_eq!(create_cache_key("img", CODE, None, &none()).len(), 64); // hex of 32 bytes
    assert_eq!(create_cache_key("img", "0", None, &none()).len(), 64);
}

#[test]
fn cache_key_empty_when_code_is_empty() {
    assert_eq!(create_cache_key("img", "", None, &none()), "");
}

#[test]
fn cache_key_empty_when_asset_type_is_empty() {
    assert_eq!(create_cache_key("", CODE, None, &none()), "");
}

#[test]
fn cache_key_empty_when_argument_is_absent() {
    // createCacheKey() defaults to assetType 'img' and encodedCode ''.
    assert_eq!(create_cache_key("img", "", None, &none()), "");
}

// ---------------- getQueryKey ----------------

const SUPPORTED: [(&str, &str); 9] = [
    ("bgColor", "c0c0c0"),
    ("width", "800"),
    ("height", "600"),
    ("scale", "1.5"),
    ("theme", "forest"),
    ("type", "png"),
    ("paper", "a0"),
    ("landscape", ""),
    ("fit", ""),
];

/// `{...supportedQuery, key: value}`; `None` drops the key (undefined/null).
fn supported_with(key: &str, value: Option<&str>) -> Query {
    SUPPORTED
        .iter()
        .filter(|(k, _)| *k != key)
        .copied()
        .chain(value.map(|v| (key, v)))
        .collect()
}

fn key(asset: &str, query: &Query) -> String {
    get_query_key(asset, Some(query), &none())
}

#[test]
fn query_key_scale_zero_or_below_keeps_dimensions() {
    assert_eq!(
        key("img", &supported_with("scale", Some("0"))),
        "bgColor=#c0c0c0,width=800,height=600,theme=forest,type=png"
    );
}

#[test]
fn query_key_scale_above_3_keeps_dimensions() {
    assert_eq!(
        key("img", &supported_with("scale", Some("3.001"))),
        "bgColor=#c0c0c0,width=800,height=600,theme=forest,type=png"
    );
}

#[test]
fn query_key_width_over_max_width_is_unchanged() {
    let l = Limits {
        max_width: JsLimit::String("800".into()),
        max_height: JsLimit::Undefined,
    };
    assert_eq!(
        get_query_key("img", Some(&q(&SUPPORTED)), &l),
        "bgColor=#c0c0c0,width=800,height=900,theme=forest,type=png"
    );
}

#[test]
fn query_key_height_over_max_height_is_unchanged() {
    let l = Limits {
        max_width: JsLimit::Undefined,
        max_height: JsLimit::String("600".into()),
    };
    assert_eq!(
        get_query_key("img", Some(&q(&SUPPORTED)), &l),
        "bgColor=#c0c0c0,width=1200,height=600,theme=forest,type=png"
    );
}

#[test]
fn query_key_img() {
    assert_eq!(
        key("img", &q(&SUPPORTED)),
        "bgColor=#c0c0c0,width=1200,height=900,theme=forest,type=png"
    );
    for ty in ["png", "webp", "jpeg", "Png", "Webp", "Jpeg"] {
        assert_eq!(
            key("img", &supported_with("type", Some(ty))),
            format!(
                "bgColor=#c0c0c0,width=1200,height=900,theme=forest,type={}",
                ty.to_lowercase()
            )
        );
    }
    assert_eq!(get_query_key("img", None, &none()), "type=jpeg");
    assert_eq!(
        key("img", &supported_with("type", Some("mp4"))),
        "bgColor=#c0c0c0,width=1200,height=900,theme=forest,type=jpeg"
    );
}

#[test]
fn query_key_svg() {
    assert_eq!(
        key("svg", &q(&SUPPORTED)),
        "bgColor=#c0c0c0,width=1200,height=900,theme=forest"
    );
    assert_eq!(get_query_key("svg", None, &none()), "");
}

#[test]
fn query_key_pdf() {
    assert_eq!(
        key("pdf", &q(&SUPPORTED)),
        "bgColor=#c0c0c0,width=1200,height=900,theme=forest,paper=a0,landscape=true,fit=true"
    );
    for paper in [
        "letter", "legal", "tabloid", "ledger", "a0", "a1", "a2", "a3", "a4", "a5", "a6", "Letter",
        "Legal", "Tabloid", "Ledger", "A0", "A1", "A2", "A3", "A4", "A5", "A6",
    ] {
        assert_eq!(
            key("pdf", &supported_with("paper", Some(paper))),
            format!(
                "bgColor=#c0c0c0,width=1200,height=900,theme=forest,paper={},landscape=true,fit=true",
                paper.to_lowercase()
            )
        );
    }
    assert_eq!(get_query_key("pdf", None, &none()), "paper=a4");
    assert_eq!(
        key("pdf", &supported_with("landscape", None)),
        "bgColor=#c0c0c0,width=1200,height=900,theme=forest,paper=a0,fit=true"
    );
    assert_eq!(
        key("pdf", &supported_with("fit", None)),
        "bgColor=#c0c0c0,width=1200,height=900,theme=forest,paper=a0,landscape=true"
    );
}
