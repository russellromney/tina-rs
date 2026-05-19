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
    pub(super) scheme: Option<String>,
    pub(super) authority: Option<String>,
    pub(super) status: Option<StatusCode>,
    pub(super) headers: HeaderMap,
    pub(super) bytes: usize,
    pub(super) saw_regular: bool,
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
    let mut out = HeaderBlock::default();
    for (name, value) in decoder
        .decode(block)
        .map_err(|_| Http2ProtocolError::HpackUnsupported)?
    {
        let name = std::str::from_utf8(&name).map_err(|_| Http2ProtocolError::HpackUnsupported)?;
        let value =
            std::str::from_utf8(&value).map_err(|_| Http2ProtocolError::HpackUnsupported)?;
        add_header(&mut out, name, value, max_header_bytes)?;
    }
    Ok(out)
}

pub(super) fn add_header(
    out: &mut HeaderBlock,
    name: &str,
    value: &str,
    max_header_bytes: usize,
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
                if out.scheme.is_some() {
                    return Err(Http2ProtocolError::InvalidPseudoHeaders);
                }
                out.scheme = Some(value.to_owned());
            }
            ":authority" => {
                if out.authority.is_some() {
                    return Err(Http2ProtocolError::InvalidPseudoHeaders);
                }
                out.authority = Some(value.to_owned());
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
    out.saw_regular = true;
    let header_name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?;
    let header_value =
        HeaderValue::from_str(value).map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?;
    if header_name == http::header::CONTENT_LENGTH {
        if out.saw_content_length {
            return Err(Http2ProtocolError::ContentLengthMismatch);
        }
        let parsed = parse_content_length(value)?;
        out.content_length = Some(parsed);
        out.saw_content_length = true;
    }
    out.headers.append(header_name, header_value);
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
    let mut block = Vec::new();
    encode_literal_header(":status", response.status.as_str(), &mut block);
    if let Some(body_len) = body_len {
        encode_literal_header("content-length", &body_len.to_string(), &mut block);
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

pub(super) fn encode_trailers(headers: &HeaderMap) -> Option<Vec<u8>> {
    let status = headers.get("grpc-status")?;
    let mut block = Vec::new();
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
    if headers.method.is_none() || headers.path.is_none() || headers.scheme.is_none() {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    let has_authority = headers.authority.as_deref().is_some_and(|v| !v.is_empty())
        || headers
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.is_empty());
    if !has_authority {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    if headers.status.is_some() {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    Ok(())
}
