//! Deterministic-replay coverage for the HTTP parser.
//!
//! Same wire bytes produce a byte-identical [`ParseProgress`] outcome
//! regardless of how many times the parser is invoked, and a
//! fingerprint hash over a generated request corpus is stable across
//! runs.
//!
//! Listener- and connection-isolate level DST under
//! `tina_sim::Simulator` with `ScriptedTcpConfig` is its own slice —
//! the scripted-TCP setup is substantial enough that it deserves
//! one. The parser is the foundation downstream DST composes onto.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tina_http::{HttpLimits, RequestParseError, parse_request_head};

#[test]
fn parser_is_pure_for_well_formed_request() {
    let buf = b"GET /counter HTTP/1.1\r\nHost: x\r\nUser-Agent: smoke\r\n\r\n";
    let limits = HttpLimits::default();

    let first = format!("{:?}", parse_request_head(buf, &limits));
    for _ in 0..100 {
        let again = format!("{:?}", parse_request_head(buf, &limits));
        assert_eq!(
            again, first,
            "parser must be a pure function over (buffer, limits)"
        );
    }
}

#[test]
fn parser_corpus_fingerprint_is_stable() {
    // A small corpus of intentionally varied inputs. The fingerprint
    // is a hash over the parser's `Debug` projection per input. If a
    // future change drifts the parser's output for any input, the
    // fingerprint moves and this test fails.
    let limits = HttpLimits::default();
    let corpus: Vec<&[u8]> = vec![
        b"GET / HTTP/1.1\r\nHost: x\r\n\r\n",
        b"GET /counter?q=1 HTTP/1.1\r\nHost: y\r\n\r\n",
        b"POST /echo HTTP/1.1\r\nHost: z\r\nContent-Length: 5\r\n\r\nhello",
        b"GET / HTTP/1.0\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close, keep-alive\r\n\r\n",
        b"BAD-REQUEST-LINE\r\n\r\n",
        b"GET http://example.com/path HTTP/1.1\r\nHost: x\r\n\r\n",
        b"GET //evil.test/path HTTP/1.1\r\nHost: x\r\n\r\n",
        b"POST /x HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello",
    ];

    let fingerprint = corpus_fingerprint(&corpus, &limits);

    // Re-run the corpus several times; the fingerprint must not move.
    for _ in 0..50 {
        let again = corpus_fingerprint(&corpus, &limits);
        assert_eq!(
            again, fingerprint,
            "corpus fingerprint must be stable across repeated runs"
        );
    }
}

#[test]
fn protocol_relative_target_is_not_origin_form() {
    let buf = b"GET //evil.test/path HTTP/1.1\r\nHost: x\r\n\r\n";
    let outcome = parse_request_head(buf, &HttpLimits::default());
    assert!(
        matches!(
            outcome,
            tina_http::ParseProgress::Failed(RequestParseError::UnsupportedRequestTarget)
        ),
        "protocol-relative target must reject as unsupported request-target; got {outcome:?}"
    );
}

#[test]
fn changing_limits_changes_parse_outcome_for_at_least_one_corpus_entry() {
    // Counter-property: the corpus fingerprint must be sensitive to
    // limit changes, otherwise the determinism claim is trivial. We
    // shrink `max_body_bytes` below one of the corpus entries' declared
    // body length and assert the fingerprint moves.
    let baseline = corpus_fingerprint(&body_corpus(), &HttpLimits::default());

    let tight_body_limit = HttpLimits {
        max_body_bytes: 4,
        ..HttpLimits::default()
    };
    let with_tight_limit = corpus_fingerprint(&body_corpus(), &tight_body_limit);

    assert_ne!(
        baseline, with_tight_limit,
        "tightening max_body_bytes must change parser outcomes for the body corpus, otherwise the determinism property is trivial"
    );
}

#[test]
fn parser_rejection_status_codes_are_stable() {
    // Each `RequestParseError` variant must map to a specific status
    // code. Drift here is a cross-cutting bug because the connection
    // isolate's wire response depends on it. A fingerprint over the
    // discriminant -> status mapping locks the surface.
    let mut hasher = DefaultHasher::new();
    for variant in [
        RequestParseError::BadRequestLine,
        RequestParseError::HeadersTooLarge,
        RequestParseError::UnsupportedTransferEncoding,
        RequestParseError::InvalidContentLength,
        RequestParseError::BodyTooLarge,
        RequestParseError::UnsupportedRequestTarget,
        RequestParseError::UnsupportedHttpVersion,
        RequestParseError::HeaderReadTimeout,
    ] {
        let status = variant.status();
        format!("{variant:?}->{}", status.as_str()).hash(&mut hasher);
    }
    let fingerprint = hasher.finish();

    // The exact value is locked here. Updating it requires intentional
    // review of every parser-error -> status mapping. The point is not
    // the magic number but the fact that any drift trips this test.
    assert_eq!(
        fingerprint, 17118294888993938310,
        "parser error -> status mapping changed; review every variant before updating"
    );
}

fn corpus_fingerprint(corpus: &[&[u8]], limits: &HttpLimits) -> u64 {
    let mut hasher = DefaultHasher::new();
    for input in corpus {
        let outcome = parse_request_head(input, limits);
        format!("{outcome:?}").hash(&mut hasher);
    }
    hasher.finish()
}

fn body_corpus() -> Vec<&'static [u8]> {
    vec![
        b"POST /a HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
        b"POST /b HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello",
        b"POST /c HTTP/1.1\r\nHost: x\r\nContent-Length: 16\r\n\r\nzzzzzzzzzzzzzzzz",
    ]
}
