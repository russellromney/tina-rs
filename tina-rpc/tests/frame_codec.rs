//! Codec roundtrip and edge-case tests for the phase 052 frame format.
//!
//! Property-based roundtrip lives at the bottom under `proptest!`.

use proptest::prelude::*;
use tina_rpc::{
    DecodeError, EncodeError, FRAME_VERSION_V1, Frame, FrameError, FrameKind, FrameLimits,
    LENGTH_PREFIX_SIZE, MAX_METHOD_LEN, MAX_SERVICE_LEN, decode, decode_body, encode,
    parse_length_prefix,
};

// MIN_BODY_SIZE is not re-exported from tina-rpc (it's an implementation
// detail). Tests below derive its value from the smallest possible encoded
// frame so they don't depend on the constant directly.
const MIN_BODY_SIZE: usize = 13;

fn limits() -> FrameLimits {
    FrameLimits::default()
}

// ----------------------------------------------------------------------------
// Golden bytes — wire format lock.
// ----------------------------------------------------------------------------

#[test]
fn golden_bytes_request_frame() {
    // This test locks the wire layout. Any silent change (field reorder,
    // endianness flip, error-code remapping, kind-byte renumber, header
    // padding) breaks it.
    let frame = Frame::request(0x0123_4567_89AB_CDEF, "svc", "m", b"hi".to_vec());
    let bytes = encode(&frame, &limits()).unwrap();
    let expected: &[u8] = &[
        // length prefix u32 BE: 13 (header) + 3 (service) + 1 (method) + 2 (payload) = 19
        0x00, 0x00, 0x00, 0x13,
        // version = 1
        0x01,
        // kind = Request (0)
        0x00,
        // error_code = 0 (None)
        0x00,
        // request_id u64 BE
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF,
        // service_len = 3, "svc"
        0x03, b's', b'v', b'c',
        // method_len = 1, "m"
        0x01, b'm',
        // payload "hi"
        b'h', b'i',
    ];
    assert_eq!(bytes.as_slice(), expected);
}

#[test]
fn golden_bytes_error_frame_locks_error_code_mapping() {
    let cases: &[(FrameError, u8)] = &[
        (FrameError::Full, 1),
        (FrameError::UnknownService, 2),
        (FrameError::UnknownMethod, 3),
        (FrameError::Decode, 4),
        (FrameError::Protocol, 5),
        (FrameError::Internal, 6),
    ];
    for (code, expected_byte) in cases {
        let frame = Frame::error(0, "", "", *code, Vec::new());
        let bytes = encode(&frame, &limits()).unwrap();
        // Layout: 4-byte length, version, kind=Error(2), error_code=N, request_id (8 zero bytes),
        // service_len=0, method_len=0, no payload.
        assert_eq!(bytes[0..4], [0x00, 0x00, 0x00, 0x0D]);
        assert_eq!(bytes[4], 0x01); // version
        assert_eq!(bytes[5], 0x02); // kind = Error
        assert_eq!(bytes[6], *expected_byte, "error code mapping for {code:?}");
    }
}

// ----------------------------------------------------------------------------
// Wire-error invariant guard. If FrameError grows a client-observed variant
// (Timeout, ConnectionClosed, …), this fn becomes non-exhaustive and breaks
// the build.
// ----------------------------------------------------------------------------

#[allow(dead_code)]
fn _frame_error_variants_are_server_reported(e: FrameError) {
    match e {
        FrameError::Full
        | FrameError::UnknownService
        | FrameError::UnknownMethod
        | FrameError::Decode
        | FrameError::Protocol
        | FrameError::Internal => {}
    }
}

// ----------------------------------------------------------------------------
// Roundtrips.
// ----------------------------------------------------------------------------

#[test]
fn request_roundtrip() {
    let frame = Frame::request(7, "billing", "charge", b"{\"cents\": 1000}".to_vec());
    let bytes = encode(&frame, &limits()).unwrap();
    let back = decode(&bytes, &limits()).unwrap();
    assert_eq!(back, frame);
}

#[test]
fn reply_roundtrip() {
    let frame = Frame::reply(7, "billing", "charge", b"ok".to_vec());
    let bytes = encode(&frame, &limits()).unwrap();
    let back = decode(&bytes, &limits()).unwrap();
    assert_eq!(back, frame);
    assert_eq!(back.error, None);
}

#[test]
fn error_roundtrip_all_codes() {
    let codes = [
        FrameError::Full,
        FrameError::UnknownService,
        FrameError::UnknownMethod,
        FrameError::Decode,
        FrameError::Protocol,
        FrameError::Internal,
    ];
    for code in codes {
        let frame = Frame::error(99, "svc", "m", code, b"why".to_vec());
        let bytes = encode(&frame, &limits()).unwrap();
        let back = decode(&bytes, &limits()).unwrap();
        assert_eq!(back, frame);
        assert_eq!(back.kind, FrameKind::Error);
        assert_eq!(back.error, Some(code));
    }
}

#[test]
fn empty_service_method_payload_roundtrip() {
    let frame = Frame::request(0, "", "", Vec::new());
    let bytes = encode(&frame, &limits()).unwrap();
    let back = decode(&bytes, &limits()).unwrap();
    assert_eq!(back, frame);
}

#[test]
fn max_service_and_method_lengths_roundtrip() {
    let service = "a".repeat(MAX_SERVICE_LEN);
    let method = "b".repeat(MAX_METHOD_LEN);
    let frame = Frame::request(1, service.clone(), method.clone(), Vec::new());
    let bytes = encode(&frame, &limits()).unwrap();
    let back = decode(&bytes, &limits()).unwrap();
    assert_eq!(back.service, service);
    assert_eq!(back.method, method);
}

#[test]
fn payload_with_arbitrary_bytes_roundtrips() {
    // Non-UTF-8 bytes are valid in the payload; only service/method are UTF-8
    // strings.
    let payload: Vec<u8> = (0u8..=255).collect();
    let frame = Frame::reply(1, "svc", "m", payload.clone());
    let bytes = encode(&frame, &limits()).unwrap();
    let back = decode(&bytes, &limits()).unwrap();
    assert_eq!(back.payload, payload);
}

#[test]
fn unicode_service_and_method_roundtrip() {
    let frame = Frame::request(1, "café", "résumé", b"data".to_vec());
    let bytes = encode(&frame, &limits()).unwrap();
    let back = decode(&bytes, &limits()).unwrap();
    assert_eq!(back.service, "café");
    assert_eq!(back.method, "résumé");
}

// ----------------------------------------------------------------------------
// Encode rejections.
// ----------------------------------------------------------------------------

#[test]
fn service_too_long_rejects_at_encode() {
    let service = "x".repeat(MAX_SERVICE_LEN + 1);
    let frame = Frame::request(1, service, "m", Vec::new());
    let err = encode(&frame, &limits()).unwrap_err();
    assert!(matches!(err, EncodeError::ServiceTooLong { len } if len == MAX_SERVICE_LEN + 1));
}

#[test]
fn method_too_long_rejects_at_encode() {
    let method = "y".repeat(MAX_METHOD_LEN + 1);
    let frame = Frame::request(1, "svc", method, Vec::new());
    let err = encode(&frame, &limits()).unwrap_err();
    assert!(matches!(err, EncodeError::MethodTooLong { len } if len == MAX_METHOD_LEN + 1));
}

#[test]
fn payload_too_large_rejects_at_encode() {
    let small = FrameLimits::new(MIN_BODY_SIZE + 8); // body fits header + tiny payload
    let frame = Frame::request(1, "svc", "m", vec![0u8; 1024]);
    let err = encode(&frame, &small).unwrap_err();
    match err {
        EncodeError::FrameTooLarge { len, max } => {
            assert!(len > max);
            assert_eq!(max, MIN_BODY_SIZE + 8);
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[test]
fn limits_below_min_rejected_at_encode() {
    let frame = Frame::request(1, "svc", "m", Vec::new());
    let err = encode(&frame, &FrameLimits::new(MIN_BODY_SIZE - 1)).unwrap_err();
    match err {
        EncodeError::LimitsTooSmall { max, min } => {
            assert_eq!(max, MIN_BODY_SIZE - 1);
            assert_eq!(min, MIN_BODY_SIZE);
        }
        other => panic!("expected LimitsTooSmall, got {other:?}"),
    }
}

#[test]
fn limits_above_u32_max_rejected_at_encode() {
    if (u32::MAX as usize).checked_add(1).is_none() {
        return; // 32-bit usize: u32::MAX + 1 wraps; skip on this target.
    }
    let frame = Frame::request(1, "svc", "m", Vec::new());
    let big = (u32::MAX as usize) + 1;
    let err = encode(&frame, &FrameLimits::new(big)).unwrap_err();
    assert!(matches!(err, EncodeError::LimitsTooLarge { max } if max == big));
}

#[test]
fn inconsistent_request_with_error_rejected_at_encode() {
    let frame = Frame {
        version: FRAME_VERSION_V1,
        kind: FrameKind::Request,
        error: Some(FrameError::Full),
        request_id: 1,
        service: "svc".to_owned(),
        method: "m".to_owned(),
        payload: Vec::new(),
    };
    let err = encode(&frame, &limits()).unwrap_err();
    assert!(matches!(err, EncodeError::InconsistentFrame));
}

#[test]
fn inconsistent_reply_with_error_rejected_at_encode() {
    let frame = Frame {
        version: FRAME_VERSION_V1,
        kind: FrameKind::Reply,
        error: Some(FrameError::Internal),
        request_id: 1,
        service: "svc".to_owned(),
        method: "m".to_owned(),
        payload: Vec::new(),
    };
    let err = encode(&frame, &limits()).unwrap_err();
    assert!(matches!(err, EncodeError::InconsistentFrame));
}

#[test]
fn inconsistent_error_without_code_rejected_at_encode() {
    let frame = Frame {
        version: FRAME_VERSION_V1,
        kind: FrameKind::Error,
        error: None,
        request_id: 1,
        service: "svc".to_owned(),
        method: "m".to_owned(),
        payload: Vec::new(),
    };
    let err = encode(&frame, &limits()).unwrap_err();
    assert!(matches!(err, EncodeError::InconsistentFrame));
}

// ----------------------------------------------------------------------------
// Decode rejections.
// ----------------------------------------------------------------------------

#[test]
fn truncated_length_prefix_rejected() {
    let err = decode(&[0u8; 3], &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::LengthPrefixTruncated));
}

#[test]
fn body_below_min_rejected_at_parse_length_prefix() {
    let prefix = ((MIN_BODY_SIZE - 1) as u32).to_be_bytes();
    let err = parse_length_prefix(prefix, &limits()).unwrap_err();
    match err {
        DecodeError::BodyTooSmall { len, min } => {
            assert_eq!(len, MIN_BODY_SIZE - 1);
            assert_eq!(min, MIN_BODY_SIZE);
        }
        other => panic!("expected BodyTooSmall, got {other:?}"),
    }
}

#[test]
fn empty_body_length_rejected() {
    let prefix = 0u32.to_be_bytes();
    let err = parse_length_prefix(prefix, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::BodyTooSmall { len: 0, .. }));
}

#[test]
fn body_too_large_rejected_before_allocation() {
    // The whole point of this test: a peer that sends only a length prefix
    // claiming a giant body must be rejected before we allocate.
    let huge = (limits().max_frame_size + 1) as u32;
    let prefix = huge.to_be_bytes();
    let err = parse_length_prefix(prefix, &limits()).unwrap_err();
    match err {
        DecodeError::BodyTooLarge { len, max } => {
            assert_eq!(len, huge as usize);
            assert_eq!(max, limits().max_frame_size);
        }
        other => panic!("expected BodyTooLarge, got {other:?}"),
    }
}

#[test]
fn limits_below_min_rejected_at_parse_length_prefix() {
    let prefix = (MIN_BODY_SIZE as u32).to_be_bytes();
    let err = parse_length_prefix(prefix, &FrameLimits::new(MIN_BODY_SIZE - 1)).unwrap_err();
    assert!(matches!(
        err,
        DecodeError::LimitsTooSmall { max, min } if max == MIN_BODY_SIZE - 1 && min == MIN_BODY_SIZE
    ));
}

#[test]
fn body_at_exactly_max_frame_size_accepted() {
    // Build a frame whose body is exactly max_frame_size bytes.
    let cap = 200;
    let payload_len = cap - MIN_BODY_SIZE - "svc".len() - "m".len();
    let frame = Frame::request(1, "svc", "m", vec![0xAB; payload_len]);
    let snug = FrameLimits::new(cap);
    let bytes = encode(&frame, &snug).unwrap();
    assert_eq!(bytes.len(), LENGTH_PREFIX_SIZE + cap);
    let back = decode(&bytes, &snug).unwrap();
    assert_eq!(back, frame);
}

#[test]
fn body_at_max_frame_size_plus_one_rejected() {
    // Same shape as above, +1 byte of payload; encode must reject.
    let cap = 200;
    let payload_len = cap - MIN_BODY_SIZE - "svc".len() - "m".len() + 1;
    let frame = Frame::request(1, "svc", "m", vec![0xAB; payload_len]);
    let snug = FrameLimits::new(cap);
    let err = encode(&frame, &snug).unwrap_err();
    match err {
        EncodeError::FrameTooLarge { len, max } => {
            assert_eq!(len, cap + 1);
            assert_eq!(max, cap);
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[test]
fn decode_full_input_oversize_rejected() {
    // Build a too-large prefix and append no body; decode() must reject before
    // touching any body bytes.
    let huge = (limits().max_frame_size + 1) as u32;
    let mut bytes = Vec::with_capacity(LENGTH_PREFIX_SIZE);
    bytes.extend_from_slice(&huge.to_be_bytes());
    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::BodyTooLarge { .. }));
}

#[test]
fn body_truncated_rejected() {
    let frame = Frame::request(1, "svc", "m", vec![1, 2, 3, 4, 5]);
    let bytes = encode(&frame, &limits()).unwrap();
    // Drop the last byte of the body.
    let truncated = &bytes[..bytes.len() - 1];
    let err = decode(truncated, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::BodyTruncated { .. }));
}

#[test]
fn body_truncated_with_zero_body_bytes() {
    // Prefix says 20 bytes but we provide only the prefix.
    let prefix = 20u32.to_be_bytes();
    let err = decode(&prefix, &limits()).unwrap_err();
    match err {
        DecodeError::BodyTruncated { expected, got } => {
            assert_eq!(expected, 20);
            assert_eq!(got, 0);
        }
        other => panic!("expected BodyTruncated, got {other:?}"),
    }
}

#[test]
fn unsupported_version_rejected() {
    let mut frame = Frame::request(1, "svc", "m", Vec::new());
    frame.version = 99;
    let bytes = encode(&frame, &limits()).unwrap();
    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::UnsupportedVersion(99)));
}

#[test]
fn unknown_kind_byte_rejected() {
    // Encode a valid frame, then corrupt the kind byte (offset 4 + 1 = 5).
    let frame = Frame::request(1, "svc", "m", Vec::new());
    let mut bytes = encode(&frame, &limits()).unwrap();
    bytes[5] = 0x55;
    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::UnknownKind(0x55)));
}

#[test]
fn unknown_error_code_byte_rejected() {
    // Encode an Error frame, then bump the error code to an unknown value.
    let frame = Frame::error(1, "svc", "m", FrameError::Full, Vec::new());
    let mut bytes = encode(&frame, &limits()).unwrap();
    bytes[6] = 0x77; // error_code at offset 4 + 2
    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::UnknownErrorCode(0x77)));
}

#[test]
fn reply_with_nonzero_error_code_rejected_at_decode() {
    // Build a Reply frame on the wire with error_code=1 manually.
    let mut bytes = encode(&Frame::reply(1, "svc", "m", Vec::new()), &limits()).unwrap();
    bytes[6] = 0x01; // overwrite error_code
    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::UnexpectedErrorCode));
}

#[test]
fn error_with_zero_error_code_rejected_at_decode() {
    // Build an Error frame on the wire and zero out its error_code.
    let mut bytes = encode(
        &Frame::error(1, "svc", "m", FrameError::Internal, Vec::new()),
        &limits(),
    )
    .unwrap();
    bytes[6] = 0x00;
    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::MissingErrorCode));
}

#[test]
fn service_length_overrun_rejected() {
    // Manually craft a body where service_len claims more bytes than exist.
    let mut body = Vec::new();
    body.push(FRAME_VERSION_V1); // version
    body.push(0); // kind = Request
    body.push(0); // error_code
    body.extend_from_slice(&1u64.to_be_bytes()); // request_id
    body.push(200); // service_len = 200, but body has only ~1 byte left
    body.push(b'x');

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&body);

    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::ServiceLengthOverrun));
}

#[test]
fn method_length_overrun_rejected() {
    // service is fine, but method_len claims more bytes than exist.
    let mut body = Vec::new();
    body.push(FRAME_VERSION_V1);
    body.push(0);
    body.push(0);
    body.extend_from_slice(&1u64.to_be_bytes());
    body.push(0); // service_len = 0
    body.push(200); // method_len = 200, only 0 bytes available

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&body);

    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::MethodLengthOverrun));
}

#[test]
fn service_not_utf8_rejected() {
    // service bytes are 0xFF 0xFF — invalid UTF-8.
    let mut body = Vec::new();
    body.push(FRAME_VERSION_V1);
    body.push(0);
    body.push(0);
    body.extend_from_slice(&1u64.to_be_bytes());
    body.push(2); // service_len = 2
    body.extend_from_slice(&[0xFF, 0xFF]);
    body.push(0); // method_len = 0

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&body);

    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::ServiceNotUtf8));
}

#[test]
fn method_not_utf8_rejected() {
    let mut body = Vec::new();
    body.push(FRAME_VERSION_V1);
    body.push(0);
    body.push(0);
    body.extend_from_slice(&1u64.to_be_bytes());
    body.push(0); // service_len = 0
    body.push(2); // method_len = 2
    body.extend_from_slice(&[0xFF, 0xFF]);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&body);

    let err = decode(&bytes, &limits()).unwrap_err();
    assert!(matches!(err, DecodeError::MethodNotUtf8));
}

// ----------------------------------------------------------------------------
// Streaming-shaped reads.
// ----------------------------------------------------------------------------

#[test]
fn parse_length_prefix_returns_body_size() {
    let frame = Frame::request(1, "svc", "m", b"hello".to_vec());
    let bytes = encode(&frame, &limits()).unwrap();
    let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
    prefix.copy_from_slice(&bytes[..LENGTH_PREFIX_SIZE]);
    let body_len = parse_length_prefix(prefix, &limits()).unwrap();
    assert_eq!(body_len, bytes.len() - LENGTH_PREFIX_SIZE);
}

#[test]
fn decode_body_recovers_frame_after_streaming_read() {
    let frame = Frame::request(123, "svc", "m", b"hello".to_vec());
    let bytes = encode(&frame, &limits()).unwrap();
    let body = &bytes[LENGTH_PREFIX_SIZE..];
    let back = decode_body(body).unwrap();
    assert_eq!(back, frame);
}

#[test]
fn small_limits_accept_minimal_frame() {
    // Smallest valid frame: empty service, empty method, empty payload.
    let frame = Frame::request(0, "", "", Vec::new());
    let body_len = encode(&frame, &limits()).unwrap().len() - LENGTH_PREFIX_SIZE;
    let snug = FrameLimits::new(body_len);
    let bytes = encode(&frame, &snug).unwrap();
    let back = decode(&bytes, &snug).unwrap();
    assert_eq!(back, frame);
}

// ----------------------------------------------------------------------------
// Property-based.
// ----------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn proptest_request_roundtrip(
        request_id in any::<u64>(),
        service in "[a-zA-Z0-9_.-]{0,255}",
        method in "[a-zA-Z0-9_.-]{0,255}",
        payload in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let frame = Frame::request(request_id, service, method, payload);
        let bytes = encode(&frame, &limits()).unwrap();
        let back = decode(&bytes, &limits()).unwrap();
        prop_assert_eq!(back, frame);
    }

    #[test]
    fn proptest_reply_roundtrip(
        request_id in any::<u64>(),
        service in "[a-zA-Z0-9_.-]{0,255}",
        method in "[a-zA-Z0-9_.-]{0,255}",
        payload in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let frame = Frame::reply(request_id, service, method, payload);
        let bytes = encode(&frame, &limits()).unwrap();
        let back = decode(&bytes, &limits()).unwrap();
        prop_assert_eq!(back, frame);
    }

    #[test]
    fn proptest_error_roundtrip(
        request_id in any::<u64>(),
        service in "[a-zA-Z0-9_.-]{0,255}",
        method in "[a-zA-Z0-9_.-]{0,255}",
        code_index in 0u8..6,
        payload in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let code = match code_index {
            0 => FrameError::Full,
            1 => FrameError::UnknownService,
            2 => FrameError::UnknownMethod,
            3 => FrameError::Decode,
            4 => FrameError::Protocol,
            _ => FrameError::Internal,
        };
        let frame = Frame::error(request_id, service, method, code, payload);
        let bytes = encode(&frame, &limits()).unwrap();
        let back = decode(&bytes, &limits()).unwrap();
        prop_assert_eq!(back, frame);
    }

    #[test]
    fn proptest_garbage_never_panics(garbage in proptest::collection::vec(any::<u8>(), 0..2048)) {
        // The decoder must reject garbage with a typed error, never panic.
        let _ = decode(&garbage, &limits());
    }

    #[test]
    fn proptest_oversize_prefix_rejected_before_allocation(
        oversize in (1024u32 * 1024 + 1)..u32::MAX,
    ) {
        let prefix = oversize.to_be_bytes();
        let err = parse_length_prefix(prefix, &limits()).unwrap_err();
        let too_large = matches!(err, DecodeError::BodyTooLarge { .. });
        prop_assert!(too_large);
    }
}
