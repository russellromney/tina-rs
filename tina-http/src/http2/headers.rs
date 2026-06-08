//! HPACK header encode/decode helpers and the typed [`HeaderBlock`].
//!
//! Internal to the `http2` module: shared between the server isolate and
//! the native client.

use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};

use super::errors::Http2ProtocolError;
use crate::HttpResponse;

pub(super) const SETTINGS_HEADER_TABLE_SIZE: u16 = 0x1;
pub(super) const SETTINGS_ENABLE_PUSH: u16 = 0x2;
pub(super) const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;
pub(super) const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 0x4;
pub(super) const SETTINGS_MAX_FRAME_SIZE: u16 = 0x5;
pub(super) const SETTINGS_MAX_HEADER_LIST_SIZE: u16 = 0x6;
pub(super) const DEFAULT_HEADER_TABLE_SIZE: u32 = 4096;
pub(super) const MIN_MAX_FRAME_SIZE: u32 = 16_384;
pub(super) const MAX_MAX_FRAME_SIZE: u32 = 16_777_215;

#[derive(Debug, Default)]
pub(super) struct HeaderBlock {
    pub(super) method: Option<Method>,
    pub(super) path: Option<String>,
    pub(super) saw_scheme: bool,
    pub(super) saw_authority: bool,
    pub(super) authority_non_empty: bool,
    pub(super) status: Option<StatusCode>,
    pub(super) headers: HeaderMap,
    pub(super) bytes: usize,
    pub(super) saw_regular: bool,
    pub(super) host_non_empty: bool,
    pub(super) grpc_content_type: bool,
    pub(super) grpc_encoding_unsupported: bool,
    /// Declared request body length, parsed once during header validation.
    /// `None` when no `content-length` header was sent.
    pub(super) content_length: Option<usize>,
    /// Set when any `content-length` header was observed during decoding,
    /// so duplicate occurrences fail closed even if the value parses.
    pub(super) saw_content_length: bool,
}

#[cfg(test)]
pub(super) fn decode_headers_block(
    block: &[u8],
    max_header_bytes: usize,
) -> Result<HeaderBlock, Http2ProtocolError> {
    let mut decoder = hpack::Decoder::new();
    decode_headers_block_with(&mut decoder, block, max_header_bytes)
}

pub(super) fn decode_headers_block_with(
    decoder: &mut hpack::Decoder<'static>,
    block: &[u8],
    max_header_bytes: usize,
) -> Result<HeaderBlock, Http2ProtocolError> {
    decode_headers_block_with_storage(decoder, block, max_header_bytes, true)
}

pub(super) fn decode_headers_block_compact_with(
    decoder: &mut hpack::Decoder<'static>,
    block: &[u8],
    max_header_bytes: usize,
) -> Result<HeaderBlock, Http2ProtocolError> {
    decode_headers_block_with_storage(decoder, block, max_header_bytes, false)
}

fn decode_headers_block_with_storage(
    decoder: &mut hpack::Decoder<'static>,
    block: &[u8],
    max_header_bytes: usize,
    store_regular_headers: bool,
) -> Result<HeaderBlock, Http2ProtocolError> {
    if let Some(headers) =
        decode_fast_literal_headers(block, max_header_bytes, store_regular_headers)?
    {
        return Ok(headers);
    }
    let mut out = HeaderBlock::default();
    for (name, value) in decoder
        .decode(block)
        .map_err(|_| Http2ProtocolError::HpackUnsupported)?
    {
        let name = std::str::from_utf8(&name).map_err(|_| Http2ProtocolError::HpackUnsupported)?;
        let value =
            std::str::from_utf8(&value).map_err(|_| Http2ProtocolError::HpackUnsupported)?;
        add_header_with_storage(
            &mut out,
            name,
            value,
            max_header_bytes,
            store_regular_headers,
        )?;
    }
    Ok(out)
}

/// Decode the common Tina/native-client HPACK shape without allocating one
/// temporary `Vec<u8>` per header name/value.
///
/// This fast path intentionally accepts only "literal header field without
/// indexing, new name" entries with plain (non-Huffman) strings — exactly what
/// [`encode_literal_header`] emits. Any indexed/dynamic/Huffman form falls back
/// to the full HPACK decoder above.
fn decode_fast_literal_headers(
    block: &[u8],
    max_header_bytes: usize,
    store_regular_headers: bool,
) -> Result<Option<HeaderBlock>, Http2ProtocolError> {
    let mut out = HeaderBlock::default();
    let mut cursor = 0;
    while cursor < block.len() {
        if block[cursor] != 0 {
            return Ok(None);
        }
        cursor += 1;
        let Some((name, used)) = decode_plain_hpack_string(&block[cursor..])? else {
            return Ok(None);
        };
        cursor += used;
        let Some((value, used)) = decode_plain_hpack_string(&block[cursor..])? else {
            return Ok(None);
        };
        cursor += used;
        add_header_with_storage(
            &mut out,
            name,
            value,
            max_header_bytes,
            store_regular_headers,
        )?;
    }
    Ok(Some(out))
}

fn decode_plain_hpack_string(input: &[u8]) -> Result<Option<(&str, usize)>, Http2ProtocolError> {
    let Some(first) = input.first().copied() else {
        return Err(Http2ProtocolError::HpackUnsupported);
    };
    if first & 0x80 != 0 {
        return Ok(None);
    }
    let (len, prefix_len) = decode_hpack_integer_7(input)?;
    let start = prefix_len;
    let end = start
        .checked_add(len)
        .ok_or(Http2ProtocolError::HpackUnsupported)?;
    if end > input.len() {
        return Err(Http2ProtocolError::HpackUnsupported);
    }
    let value = std::str::from_utf8(&input[start..end])
        .map_err(|_| Http2ProtocolError::HpackUnsupported)?;
    Ok(Some((value, end)))
}

fn decode_hpack_integer_7(input: &[u8]) -> Result<(usize, usize), Http2ProtocolError> {
    let first = input
        .first()
        .copied()
        .ok_or(Http2ProtocolError::HpackUnsupported)?
        & 0x7f;
    if first < 0x7f {
        return Ok((first as usize, 1));
    }
    let mut value = 0x7f_usize;
    let mut shift = 0usize;
    let mut cursor = 1usize;
    loop {
        let byte = input
            .get(cursor)
            .copied()
            .ok_or(Http2ProtocolError::HpackUnsupported)?;
        cursor += 1;
        let part = (byte & 0x7f) as usize;
        value = value
            .checked_add(
                part.checked_shl(shift as u32)
                    .ok_or(Http2ProtocolError::HpackUnsupported)?,
            )
            .ok_or(Http2ProtocolError::HpackUnsupported)?;
        if byte & 0x80 == 0 {
            return Ok((value, cursor));
        }
        shift = shift
            .checked_add(7)
            .ok_or(Http2ProtocolError::HpackUnsupported)?;
        if shift >= usize::BITS as usize {
            return Err(Http2ProtocolError::HpackUnsupported);
        }
    }
}

#[cfg(test)]
pub(super) fn add_header(
    out: &mut HeaderBlock,
    name: &str,
    value: &str,
    max_header_bytes: usize,
) -> Result<(), Http2ProtocolError> {
    add_header_with_storage(out, name, value, max_header_bytes, true)
}

fn add_header_with_storage(
    out: &mut HeaderBlock,
    name: &str,
    value: &str,
    max_header_bytes: usize,
    store_regular_headers: bool,
) -> Result<(), Http2ProtocolError> {
    out.bytes = out
        .bytes
        .checked_add(name.len() + value.len())
        .ok_or(Http2ProtocolError::HeadersTooLarge)?;
    if out.bytes > max_header_bytes {
        return Err(Http2ProtocolError::HeadersTooLarge);
    }
    if name.starts_with(':') {
        if out.saw_regular {
            return Err(Http2ProtocolError::InvalidPseudoHeaders);
        }
        match name {
            ":method" => {
                if out.method.is_some() {
                    return Err(Http2ProtocolError::InvalidPseudoHeaders);
                }
                out.method = Some(
                    Method::from_bytes(value.as_bytes())
                        .map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?,
                );
            }
            ":path" => {
                if out.path.is_some() {
                    return Err(Http2ProtocolError::InvalidPseudoHeaders);
                }
                out.path = Some(value.to_owned());
            }
            ":scheme" => {
                if out.saw_scheme {
                    return Err(Http2ProtocolError::InvalidPseudoHeaders);
                }
                out.saw_scheme = true;
            }
            ":authority" => {
                if out.saw_authority {
                    return Err(Http2ProtocolError::InvalidPseudoHeaders);
                }
                out.saw_authority = true;
                out.authority_non_empty = !value.is_empty();
            }
            ":status" => {
                if out.status.is_some() {
                    return Err(Http2ProtocolError::InvalidPseudoHeaders);
                }
                out.status = Some(
                    StatusCode::from_bytes(value.as_bytes())
                        .map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?,
                );
            }
            _ => return Err(Http2ProtocolError::InvalidPseudoHeaders),
        }
        return Ok(());
    }
    if name.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    if matches!(
        name,
        "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
    ) {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    if name == "te" && !value.trim().eq_ignore_ascii_case("trailers") {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    out.saw_regular = true;
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?;
    let header_value =
        HeaderValue::from_str(value).map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?;
    if header_name == http::header::HOST {
        out.host_non_empty = !value.is_empty();
    }
    if header_name == http::header::CONTENT_TYPE {
        out.grpc_content_type = crate::grpc::is_grpc_content_type(value);
    }
    if name == "grpc-encoding" && !value.eq_ignore_ascii_case("identity") {
        out.grpc_encoding_unsupported = true;
    }
    if header_name == http::header::CONTENT_LENGTH {
        if out.saw_content_length {
            return Err(Http2ProtocolError::ContentLengthMismatch);
        }
        let parsed = parse_content_length(value)?;
        out.content_length = Some(parsed);
        out.saw_content_length = true;
    }
    if store_regular_headers {
        out.headers.append(header_name, header_value);
    }
    Ok(())
}

pub(super) fn parse_content_length(value: &str) -> Result<usize, Http2ProtocolError> {
    // RFC 9110 §8.6: content-length is a single nonnegative decimal integer.
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Http2ProtocolError::ContentLengthMismatch);
    }
    value
        .parse::<usize>()
        .map_err(|_| Http2ProtocolError::ContentLengthMismatch)
}

pub(super) fn encode_literal_header(name: &str, value: &str, out: &mut Vec<u8>) {
    out.push(0);
    encode_string(name, out);
    encode_string(value, out);
}

pub(super) fn encode_string(value: &str, out: &mut Vec<u8>) {
    encode_integer(value.len(), 7, 0, out);
    out.extend_from_slice(value.as_bytes());
}

pub(super) fn encode_integer(mut value: usize, prefix_bits: u8, pattern: u8, out: &mut Vec<u8>) {
    let max = (1_usize << prefix_bits) - 1;
    if value < max {
        out.push(pattern | value as u8);
        return;
    }
    out.push(pattern | max as u8);
    value -= max;
    while value >= 128 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(super) fn encode_response_headers(response: &HttpResponse, body_len: usize) -> Vec<u8> {
    encode_response_headers_with_len(response, Some(body_len))
}

pub(super) fn encode_response_headers_with_len(
    response: &HttpResponse,
    body_len: Option<usize>,
) -> Vec<u8> {
    // Pre-size so a small header block does not pay several growth
    // reallocations — each `Vec` realloc is a counted allocation under the perf
    // allocator (the default `GlobalAlloc::realloc` calls `alloc`).
    let mut block = Vec::with_capacity(96);
    encode_literal_header(":status", response.status.as_str(), &mut block);
    if let Some(body_len) = body_len {
        // Format content-length into a stack buffer instead of a heap `String`.
        let mut len_buf = [0u8; 20];
        encode_literal_header(
            "content-length",
            format_usize(&mut len_buf, body_len),
            &mut block,
        );
    }
    for (name, value) in response.headers.iter() {
        if name.as_str().starts_with(':') {
            continue;
        }
        if name.as_str() == "grpc-status"
            || name.as_str() == "grpc-message"
            || name.as_str() == "content-length"
            || name.as_str() == "transfer-encoding"
        {
            continue;
        }
        if let Ok(value) = value.to_str() {
            encode_literal_header(name.as_str(), value, &mut block);
        }
    }
    block
}

/// Format a `usize` as decimal ASCII into a stack buffer, returning a `&str`
/// view — no heap `String`. The buffer must be at least 20 bytes (fits
/// `usize::MAX`).
fn format_usize(buf: &mut [u8; 20], mut n: usize) -> &str {
    let mut i = buf.len();
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    // SAFETY-free: only ASCII digits were written into `buf[i..]`.
    std::str::from_utf8(&buf[i..]).expect("ascii digits are valid utf-8")
}

pub(super) fn encode_trailers(headers: &HeaderMap) -> Option<Vec<u8>> {
    let status = headers.get("grpc-status")?;
    let mut block = Vec::with_capacity(64);
    if let Ok(value) = status.to_str() {
        encode_literal_header("grpc-status", value, &mut block);
    }
    if let Some(message) = headers.get("grpc-message") {
        if let Ok(value) = message.to_str() {
            encode_literal_header("grpc-message", value, &mut block);
        }
    }
    Some(block)
}

pub(super) fn encode_response_trailers(response: &HttpResponse) -> Option<Vec<u8>> {
    encode_trailers(&response.headers)
}

/// Validate the required pseudo-headers and authority/Host rule for an inbound
/// HTTP/2 request. `:status` must not appear on requests.
pub(super) fn validate_request_headers(headers: &HeaderBlock) -> Result<(), Http2ProtocolError> {
    if headers.method.is_none() || !headers.saw_scheme {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    let Some(path) = headers.path.as_deref() else {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    };
    if !is_valid_request_path(path) {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    let has_authority = headers.authority_non_empty || headers.host_non_empty;
    if !has_authority {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    if headers.status.is_some() {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    Ok(())
}

fn is_valid_request_path(path: &str) -> bool {
    // Tina's first HTTP/2 server accepts origin-form request paths.
    // CONNECT/asterisk/empty targets are not first-form features. Raw
    // whitespace/control bytes are rejected so a path cannot smuggle
    // ambiguous downstream routing text.
    path.starts_with('/') && path.bytes().all(|byte| byte > b' ' && byte != 0x7f)
}

/// Validate the required pseudo-headers for an inbound HTTP/2 response.
/// `:status` must appear; request pseudo-headers must not.
pub(super) fn validate_response_headers(headers: &HeaderBlock) -> Result<(), Http2ProtocolError> {
    if headers.status.is_none() {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    if headers.method.is_some()
        || headers.path.is_some()
        || headers.saw_scheme
        || headers.saw_authority
    {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    Ok(())
}

/// Validate an inbound trailer block (the second HEADERS on a response
/// stream). RFC 9113 §8.1 forbids pseudo-headers, `content-length`, and
/// connection-control headers in trailers. Returns
/// [`Http2ProtocolError::InvalidTrailerPseudoHeader`] when a pseudo-header
/// is present, [`Http2ProtocolError::ContentLengthMismatch`] when a
/// `content-length` trailer is observed.
pub(super) fn validate_trailer_block(headers: &HeaderBlock) -> Result<(), Http2ProtocolError> {
    if headers.method.is_some()
        || headers.path.is_some()
        || headers.saw_scheme
        || headers.saw_authority
        || headers.status.is_some()
    {
        return Err(Http2ProtocolError::InvalidTrailerPseudoHeader);
    }
    if headers.saw_content_length {
        return Err(Http2ProtocolError::ContentLengthMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_request_headers(path: &str) -> HeaderBlock {
        let mut headers = HeaderBlock::default();
        add_header(&mut headers, ":method", "GET", 1024).unwrap();
        add_header(&mut headers, ":scheme", "http", 1024).unwrap();
        add_header(&mut headers, ":authority", "example.com", 1024).unwrap();
        add_header(&mut headers, ":path", path, 1024).unwrap();
        headers
    }

    #[test]
    fn fast_literal_header_decode_matches_tina_encoder() {
        let mut block = Vec::new();
        encode_literal_header(":method", "POST", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":authority", "example.com", &mut block);
        encode_literal_header(":path", "/pkg.Service/Unary", &mut block);
        encode_literal_header("content-type", "application/grpc", &mut block);
        encode_literal_header("content-length", "9", &mut block);

        let decoded = decode_headers_block(&block, 1024).unwrap();
        assert_eq!(decoded.method, Some(Method::POST));
        assert_eq!(decoded.path.as_deref(), Some("/pkg.Service/Unary"));
        assert!(decoded.saw_scheme);
        assert!(decoded.saw_authority);
        assert!(decoded.authority_non_empty);
        assert_eq!(decoded.content_length, Some(9));
        assert_eq!(
            decoded
                .headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/grpc")
        );
    }

    #[test]
    fn fast_literal_header_decode_handles_extended_string_lengths() {
        let path = format!("/{}", "x".repeat(180));
        let mut block = Vec::new();
        encode_literal_header(":method", "GET", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":authority", "example.com", &mut block);
        encode_literal_header(":path", &path, &mut block);

        let decoded = decode_headers_block(&block, 4096).unwrap();
        assert_eq!(decoded.path.as_deref(), Some(path.as_str()));
        validate_request_headers(&decoded).unwrap();
    }

    #[test]
    fn indexed_header_block_falls_back_to_full_hpack_decoder() {
        let mut block = Vec::new();
        block.push(0x82); // static table: :method GET
        block.push(0x86); // static table: :scheme http
        block.push(0x84); // static table: :path /
        encode_literal_header(":authority", "example.com", &mut block);

        let decoded = decode_headers_block(&block, 1024).unwrap();
        assert_eq!(decoded.method, Some(Method::GET));
        assert_eq!(decoded.path.as_deref(), Some("/"));
        assert!(decoded.saw_scheme);
        assert!(decoded.authority_non_empty);
        validate_request_headers(&decoded).unwrap();
    }

    #[test]
    fn compact_header_decode_keeps_facts_without_storing_public_headers() {
        let mut block = Vec::new();
        encode_literal_header(":method", "POST", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":path", "/pkg.Service/Unary", &mut block);
        encode_literal_header("host", "example.com", &mut block);
        encode_literal_header("content-type", "application/grpc+proto", &mut block);
        encode_literal_header("grpc-encoding", "gzip", &mut block);
        encode_literal_header("content-length", "9", &mut block);

        let mut decoder = hpack::Decoder::new();
        let decoded = decode_headers_block_compact_with(&mut decoder, &block, 1024).unwrap();
        assert_eq!(decoded.path.as_deref(), Some("/pkg.Service/Unary"));
        assert!(decoded.host_non_empty);
        assert!(decoded.grpc_content_type);
        assert!(decoded.grpc_encoding_unsupported);
        assert_eq!(decoded.content_length, Some(9));
        assert!(decoded.headers.is_empty());
        validate_request_headers(&decoded).unwrap();
    }

    #[test]
    fn request_path_pseudo_header_must_not_be_empty() {
        let headers = valid_request_headers("");
        assert_eq!(
            validate_request_headers(&headers),
            Err(Http2ProtocolError::InvalidPseudoHeaders)
        );
    }

    #[test]
    fn request_path_pseudo_header_must_be_origin_form() {
        for path in ["pkg.Service/Unary", "*", "http://example.com/x"] {
            let headers = valid_request_headers(path);
            assert_eq!(
                validate_request_headers(&headers),
                Err(Http2ProtocolError::InvalidPseudoHeaders),
                "path {path:?} must reject"
            );
        }
    }

    #[test]
    fn request_path_pseudo_header_rejects_control_bytes() {
        let headers = valid_request_headers("/safe\r\nbad");
        assert_eq!(
            validate_request_headers(&headers),
            Err(Http2ProtocolError::InvalidPseudoHeaders)
        );
    }
}
