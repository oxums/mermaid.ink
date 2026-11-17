//! Property tests for invariants the vectors cannot enumerate.

use mink_core::decode::{DecodeError, decode, prepare};
use mink_core::query::{Limits, Query, extract_dimension, validate_query};
use proptest::prelude::*;

proptest! {
    #[test]
    fn decode_and_prepare_never_panic(s in any::<String>(), theme in proptest::option::of("[a-z]{1,8}")) {
        if let Ok(state) = decode(&s, 1 << 20) {
            let _ = prepare(state, theme.as_deref());
        }
    }

    #[test]
    fn decode_never_panics_on_base64ish_payloads(s in "(pako:|base64:)?[A-Za-z0-9+/=_-]{0,200}") {
        let _ = decode(&s, 64);
    }

    #[test]
    fn input_without_colon_is_never_unknown_serde(s in "[^:]*") {
        prop_assert!(!matches!(decode(&s, 1 << 20), Err(DecodeError::UnknownSerde(_))));
    }

    #[test]
    fn validated_dimensions_are_positive_or_absent(
        raw in "((width|height|scale)=[ +\\-.0-9eExa-z%]{0,8}&?){0,4}",
        max_w in proptest::option::of("[0-9a-z]{0,6}"),
        max_h in proptest::option::of("[0-9a-z]{0,6}"),
    ) {
        let q = Query::parse(&raw);
        let limits = Limits::from_env_values(max_w.as_deref(), max_h.as_deref());
        if validate_query(&q, &limits).is_ok() {
            let d = extract_dimension(Some(&q), &limits).unwrap();
            for v in [d.width, d.height].into_iter().flatten() {
                prop_assert!(v > 0.0 && v.is_finite(), "{raw:?} -> {d:?}");
            }
        }
    }
}
