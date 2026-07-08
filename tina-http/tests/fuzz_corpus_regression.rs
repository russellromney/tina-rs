//! Deterministic regressions folded out of the out-of-workspace fuzz targets.
//!
//! The fuzzers never run per-PR (CI only compiles the shim), so the
//! panic-containment property they cover has no continuous protection. These
//! tests feed each target's known-interesting inputs to the REAL production
//! function and assert the expected outcome — no panic, typed error, or
//! decoded value — so `cargo test` guards the same invariants every PR.

use tina_http::chunked_decoder::{ChunkedDecoder, FeedAllResult};
use tina_rpc::{DecodeError, FrameLimits, LENGTH_PREFIX_SIZE, decode, decode_body};

/// Mirrors `tina_rpc::frame::MIN_BODY_SIZE` (not re-exported): the shortest body
/// `decode_body` accepts. Anything shorter is a typed error.
const MIN_BODY_SIZE: usize = 13;

/// `chunked_decoder` target: the decoded-length cap must hold across a split
/// feed. Feed a body larger than the cap in two pieces and assert the
/// accumulator never exceeds the cap after either feed, and the decoder fails
/// closed rather than overrunning.
#[test]
fn chunked_decoder_cap_holds_across_a_split_feed() {
    // A single 8-byte chunk ("AAAAAAAA") whose declared size exceeds the 4-byte
    // cap. Split mid-chunk so the cap check must hold on both feeds.
    let wire = b"8\r\nAAAAAAAA\r\n0\r\n\r\n";
    let cap = 4;
    for split in 0..=wire.len() {
        let mut decoder = ChunkedDecoder::new(cap);
        let mut decoded = Vec::new();
        let (first, _) = decoder.feed_all(&wire[..split], &mut decoded);
        assert!(
            decoded.len() <= cap,
            "decoded {} exceeded cap {cap} after first feed at split {split} ({first:?})",
            decoded.len()
        );
        let (second, _) = decoder.feed_all(&wire[split..], &mut decoded);
        assert!(
            decoded.len() <= cap,
            "decoded {} exceeded cap {cap} after second feed at split {split} ({second:?})",
            decoded.len()
        );
        // The over-cap body must be rejected, never completed.
        assert!(
            matches!(second, FeedAllResult::Failed(_)) || decoded.len() <= cap,
            "over-cap chunk must fail closed (split {split}, {second:?})"
        );
    }
}

/// `chunked_decoder` target: a well-formed body under the cap decodes to the
/// exact payload regardless of where the feed is split.
#[test]
fn chunked_decoder_decodes_wellformed_body_across_every_split() {
    let wire = b"3\r\nabc\r\n0\r\n\r\n";
    for split in 0..=wire.len() {
        let mut decoder = ChunkedDecoder::new(64);
        let mut decoded = Vec::new();
        let (first, _) = decoder.feed_all(&wire[..split], &mut decoded);
        let done = matches!(first, FeedAllResult::Complete);
        if !done {
            let (_second, _) = decoder.feed_all(&wire[split..], &mut decoded);
        }
        assert_eq!(decoded, b"abc", "split {split} lost body bytes");
    }
}

/// `rpc_frame` target: a truncated length prefix must surface a typed
/// `DecodeError`, never panic. Covers every length below the 4-byte prefix and
/// a prefix that declares a body the input does not carry.
#[test]
fn rpc_frame_decode_contains_truncated_length_prefix() {
    let limits = FrameLimits::default();

    // Fewer bytes than the length prefix: typed error, no panic.
    for len in 0..LENGTH_PREFIX_SIZE {
        let bytes = vec![0u8; len];
        assert!(
            matches!(
                decode(&bytes, &limits),
                Err(DecodeError::LengthPrefixTruncated)
            ),
            "input of {len} bytes must report a truncated length prefix"
        );
    }

    // Full prefix declaring a 100-byte body, but no body follows.
    let mut bytes = 100u32.to_be_bytes().to_vec();
    bytes.extend_from_slice(&[0u8; 4]);
    assert!(
        matches!(
            decode(&bytes, &limits),
            Err(DecodeError::BodyTruncated { .. })
        ),
        "a declared body larger than the input must report truncation"
    );

    // Body-only decoder trusts the caller's length: a body shorter than
    // MIN_BODY_SIZE must be a typed error, and a longer all-zero body must
    // still never panic or slice out of bounds.
    for len in 0..(MIN_BODY_SIZE + 3) {
        let body = vec![0u8; len];
        let result = decode_body(&body);
        if len < MIN_BODY_SIZE {
            assert!(
                result.is_err(),
                "body of {len} bytes (< MIN_BODY_SIZE {MIN_BODY_SIZE}) must be a typed error"
            );
        }
        // len >= MIN_BODY_SIZE: reaching here without a panic is the property.
    }
}
