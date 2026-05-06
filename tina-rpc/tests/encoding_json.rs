//! Encoding adapter tests. First impl: JSON.

use serde::{Deserialize, Serialize};
use tina_rpc::{Encoding, EncodingError, EncodingErrorKind, Json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChargeRequest {
    cents: u64,
    customer: String,
    items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AuditEvent {
    Created { id: u64 },
    Closed { id: u64, reason: String },
}

const HUGE: usize = 16 * 1024 * 1024;

// ----------------------------------------------------------------------------
// Roundtrips.
// ----------------------------------------------------------------------------

#[test]
fn json_roundtrip_struct() {
    let req = ChargeRequest {
        cents: 12_500,
        customer: "ada".to_owned(),
        items: vec!["coffee".to_owned(), "scone".to_owned()],
    };
    let bytes = Json.encode(&req, HUGE).unwrap();
    let back: ChargeRequest = Json.decode(&bytes, HUGE).unwrap();
    assert_eq!(back, req);
}

#[test]
fn json_roundtrip_enum() {
    let event = AuditEvent::Closed {
        id: 7,
        reason: "duplicate".to_owned(),
    };
    let bytes = Json.encode(&event, HUGE).unwrap();
    let back: AuditEvent = Json.decode(&bytes, HUGE).unwrap();
    assert_eq!(back, event);
}

#[test]
fn json_roundtrip_primitives() {
    let bytes = Json.encode(&42u64, HUGE).unwrap();
    let back: u64 = Json.decode(&bytes, HUGE).unwrap();
    assert_eq!(back, 42);

    let s = "hello".to_owned();
    let bytes = Json.encode(&s, HUGE).unwrap();
    let back: String = Json.decode(&bytes, HUGE).unwrap();
    assert_eq!(back, "hello");

    let v = vec![1u32, 2, 3];
    let bytes = Json.encode(&v, HUGE).unwrap();
    let back: Vec<u32> = Json.decode(&bytes, HUGE).unwrap();
    assert_eq!(back, v);
}

#[test]
fn json_roundtrip_unit() {
    let bytes = Json.encode(&(), HUGE).unwrap();
    assert_eq!(&bytes, b"null");
    let _: () = Json.decode(&bytes, HUGE).unwrap();
    let _: () = Json.decode(&bytes, bytes.len()).unwrap();
}

// ----------------------------------------------------------------------------
// Size limits.
// ----------------------------------------------------------------------------

#[test]
fn encode_too_large_rejected_during_serialization() {
    // Caps the encoder mid-stream rather than producing a giant Vec first.
    let big = "x".repeat(10 * 1024 * 1024);
    let err = Json.encode(&big, 16).unwrap_err();
    match err {
        EncodingError::PayloadTooLarge { len, max } => {
            assert!(len > max);
            assert_eq!(max, 16);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }
}

#[test]
fn decode_too_large_rejected_before_deserialize() {
    let s = "x".repeat(1024);
    let bytes = Json.encode(&s, HUGE).unwrap();
    let err = Json.decode::<String>(&bytes, 16).unwrap_err();
    match err {
        EncodingError::PayloadTooLarge { len, max } => {
            assert_eq!(len, bytes.len());
            assert_eq!(max, 16);
        }
        other => panic!("expected PayloadTooLarge, got {other:?}"),
    }
}

#[test]
fn encode_at_exactly_max_size_accepted() {
    let s = "x".to_owned();
    let bytes = Json.encode(&s, HUGE).unwrap();
    let exact_cap = bytes.len();
    let bytes_again = Json.encode(&s, exact_cap).unwrap();
    assert_eq!(bytes_again, bytes);
}

#[test]
fn decode_at_exactly_max_size_accepted() {
    let s = "x".to_owned();
    let bytes = Json.encode(&s, HUGE).unwrap();
    let exact_cap = bytes.len();
    let back: String = Json.decode(&bytes, exact_cap).unwrap();
    assert_eq!(back, s);
}

// ----------------------------------------------------------------------------
// Decode failure classification.
// ----------------------------------------------------------------------------

#[test]
fn decode_malformed_json_classified_as_syntax() {
    let garbage = b"{not really json}";
    let err = Json.decode::<ChargeRequest>(garbage, HUGE).unwrap_err();
    assert_eq!(
        err,
        EncodingError::Decode {
            encoder: "json",
            kind: EncodingErrorKind::Syntax,
        }
    );
}

#[test]
fn decode_wrong_shape_classified_as_data_mismatch() {
    // Valid JSON but wrong shape for ChargeRequest.
    let bytes = Json.encode(&"just a string", HUGE).unwrap();
    let err = Json.decode::<ChargeRequest>(&bytes, HUGE).unwrap_err();
    assert_eq!(
        err,
        EncodingError::Decode {
            encoder: "json",
            kind: EncodingErrorKind::DataMismatch,
        }
    );
}

#[test]
fn decode_empty_input_classified_as_unexpected_eof() {
    let err = Json.decode::<ChargeRequest>(&[], HUGE).unwrap_err();
    assert_eq!(
        err,
        EncodingError::Decode {
            encoder: "json",
            kind: EncodingErrorKind::UnexpectedEof,
        }
    );
}

#[test]
fn decode_trailing_data_rejected() {
    let mut bytes = Json.encode(&42u64, HUGE).unwrap();
    bytes.extend_from_slice(b" garbage");
    let err = Json.decode::<u64>(&bytes, HUGE).unwrap_err();
    // serde_json reports trailing data as Syntax.
    assert!(matches!(
        err,
        EncodingError::Decode {
            encoder: "json",
            kind: EncodingErrorKind::Syntax,
        }
    ));
}

#[test]
fn decode_utf8_bom_rejected() {
    // serde_json does not accept a UTF-8 BOM at the start of input.
    let bytes = b"\xEF\xBB\xBF42";
    let err = Json.decode::<u64>(bytes, HUGE).unwrap_err();
    assert!(matches!(
        err,
        EncodingError::Decode {
            encoder: "json",
            kind: EncodingErrorKind::Syntax,
        }
    ));
}

#[test]
fn decode_invalid_utf8_inside_string_rejected() {
    // Construct: "\xFF\xFE" wrapped as a JSON string. \x22 = ".
    let bytes = b"\x22\xFF\xFE\x22";
    let err = Json.decode::<String>(bytes, HUGE).unwrap_err();
    assert!(matches!(
        err,
        EncodingError::Decode {
            encoder: "json",
            kind: EncodingErrorKind::Syntax,
        }
    ));
}

#[test]
fn decode_deeply_nested_returns_typed_error_not_panic() {
    // serde_json has a built-in recursion limit; exceeding it must surface as
    // a typed error, never a panic or stack overflow.
    let depth = 200;
    let mut deep = String::with_capacity(depth * 2 + 1);
    for _ in 0..depth {
        deep.push('[');
    }
    deep.push('0');
    for _ in 0..depth {
        deep.push(']');
    }
    let result: Result<serde_json::Value, _> = Json.decode(deep.as_bytes(), HUGE);
    let err = result.unwrap_err();
    // serde_json classifies recursion-limit hits as Syntax errors.
    assert!(matches!(
        err,
        EncodingError::Decode {
            encoder: "json",
            kind: EncodingErrorKind::Syntax,
        }
    ));
}

// ----------------------------------------------------------------------------
// Encode failure classification.
// ----------------------------------------------------------------------------

#[test]
fn encode_unsupported_value_classified() {
    // serde_json cannot represent maps keyed by tuples (no string coercion).
    use std::collections::BTreeMap;
    let mut bad: BTreeMap<(u8, u8), String> = BTreeMap::new();
    bad.insert((1, 2), "pair".to_owned());
    let err = Json.encode(&bad, HUGE).unwrap_err();
    match err {
        EncodingError::Encode { encoder, kind } => {
            assert_eq!(encoder, "json");
            // Unsupported map keys are classified as Other (custom Serialize
            // error) by serde_json — we only care that it is *some* Encode
            // variant with the right encoder name.
            let _ = kind;
        }
        other => panic!("expected Encode, got {other:?}"),
    }
}

// ----------------------------------------------------------------------------
// API shape.
// ----------------------------------------------------------------------------

#[test]
fn json_default_is_zero_sized() {
    assert_eq!(std::mem::size_of::<Json>(), 0);
}

#[test]
fn encoding_name_is_stable() {
    assert_eq!(Json.name(), "json");
}
