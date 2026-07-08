//! HPACK header encode/decode helpers and the typed [`HeaderBlock`].
//!
//! Internal to the `http2` module: shared between the server isolate and
//! the native client.

use std::sync::Arc;

use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};

use super::errors::Http2ProtocolError;
use crate::HttpResponse;

/// Bounded request-path cache, one per server connection. Warmed routes repeat
/// the same method path every call; a hit hands back a cloned `Arc<str>` (a
/// refcount bump, no allocation) instead of a fresh owned path.
///
/// Decode only [`lookup`](Self::lookup)s (read-only); a path is [`remember`](
/// Self::remember)ed only after its request validates, so a peer cannot fill the
/// cache with unvalidated paths. The cache is capped and uses least-recently-used
/// admission: once full it evicts the oldest entry, so a warmed route is still
/// admitted under a flood of one-shot paths. `entries[0]` is most-recently-used.
#[derive(Debug)]
pub(super) struct PathInternCache {
    entries: Vec<Arc<str>>,
    cap: usize,
    evictions: u64,
}

impl PathInternCache {
    pub(super) fn with_capacity(cap: usize) -> Self {
        Self {
            entries: Vec::new(),
            cap,
            evictions: 0,
        }
    }

    /// Number of cached paths evicted because the cache was full. Surfaced in
    /// the connection report so a churning, saturated cache is observable.
    pub(super) fn evictions(&self) -> u64 {
        self.evictions
    }

    /// Return the cached `Arc<str>` for `path`, or `None`. Read-only: decode
    /// looks up without mutating, so an unvalidated request cannot poison the
    /// cache. A connection serves a handful of routes, so a linear scan wins.
    pub(super) fn lookup(&self, path: &str) -> Option<Arc<str>> {
        self.entries
            .iter()
            .find(|entry| &***entry == path)
            .map(Arc::clone)
    }

    /// Cache a validated request path so later warmed calls reuse it. Already
    /// present: mark it most-recently-used. Absent and full: evict the
    /// least-recently-used entry so the warmed route is admitted.
    pub(super) fn remember(&mut self, path: &Arc<str>) {
        let path_str: &str = path;
        if let Some(pos) = self.entries.iter().position(|entry| &**entry == path_str) {
            if pos != 0 {
                let entry = self.entries.remove(pos);
                self.entries.insert(0, entry);
            }
            return;
        }
        if self.entries.len() >= self.cap {
            self.entries.pop();
            self.evictions = self.evictions.saturating_add(1);
        }
        self.entries.insert(0, Arc::clone(path));
    }
}

/// Build an owned request path, reusing the cached `Arc` on a hit and falling
/// back to a fresh `Arc` on a miss (or when no cache is supplied — the client
/// response path, which never carries `:path`). Never mutates the cache; the
/// caller caches the path only after the request validates.
fn own_request_path(cache: Option<&PathInternCache>, value: &str) -> Arc<str> {
    match cache {
        Some(cache) => cache.lookup(value).unwrap_or_else(|| Arc::from(value)),
        None => Arc::from(value),
    }
}

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
    pub(super) path: Option<Arc<str>>,
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
    /// Raw `grpc-status` code, if the block carried one. Captured as a fact so
    /// the compact gRPC client response path can read the final status without
    /// building a public `HeaderMap`. `None` for non-gRPC blocks.
    pub(super) grpc_status: Option<u16>,
    /// Set once a `grpc-status` header is seen, so duplicates keep the first
    /// value — matching `HeaderMap::get` on the public path.
    pub(super) saw_grpc_status: bool,
    /// Raw (still percent-encoded) `grpc-message`, if present. Only gRPC error
    /// responses carry this, so the `String` is allocated only on errors.
    pub(super) grpc_message: Option<String>,
}

#[cfg(test)]
pub(super) fn decode_headers_block(
    block: &[u8],
    max_header_bytes: usize,
) -> Result<HeaderBlock, Http2ProtocolError> {
    let mut decoder = hpack::Decoder::new();
    decode_headers_block_with(&mut decoder, block, max_header_bytes, None)
}

pub(super) fn decode_headers_block_with(
    decoder: &mut hpack::Decoder<'static>,
    block: &[u8],
    max_header_bytes: usize,
    path_cache: Option<&PathInternCache>,
) -> Result<HeaderBlock, Http2ProtocolError> {
    decode_headers_block_with_storage(decoder, block, max_header_bytes, true, path_cache)
}

pub(super) fn decode_headers_block_compact_with(
    decoder: &mut hpack::Decoder<'static>,
    block: &[u8],
    max_header_bytes: usize,
    path_cache: Option<&PathInternCache>,
) -> Result<HeaderBlock, Http2ProtocolError> {
    decode_headers_block_with_storage(decoder, block, max_header_bytes, false, path_cache)
}

/// HPACK integer decode (RFC 7541 §5.1) mirroring `hpack`'s own
/// `decode_integer`, including its 5-octet ceiling. Returns `(value, consumed)`
/// on success, or `None` for the exact inputs that make `hpack` return `Err`:
/// an empty buffer, an unterminated continuation, or a too-long encoding.
fn hpack_take_integer(buf: &[u8], prefix_bits: u8) -> Option<(usize, usize)> {
    const OCTET_LIMIT: usize = 5;
    let &first = buf.first()?;
    let mask = (1usize << prefix_bits) - 1;
    let mut value = usize::from(first) & mask;
    if value < mask {
        return Some((value, 1));
    }
    let mut total = 1;
    let mut shift = 0u32;
    for &b in &buf[1..] {
        total += 1;
        value = value.checked_add(usize::from(b & 0x7f).checked_shl(shift)?)?;
        shift += 7;
        if b & 0x80 == 0 {
            return Some((value, total));
        }
        if total == OCTET_LIMIT {
            // Too many octets: `hpack` returns `Err` here, which its
            // size-update path then unwraps into a panic.
            return None;
        }
    }
    None
}

/// Advances past one HPACK string: a length integer (7-bit prefix) followed by
/// that many raw octets. Returns the remaining buffer, or `None` if the length
/// integer or the octets run past the end.
fn hpack_skip_string(buf: &[u8]) -> Option<&[u8]> {
    let (len, consumed) = hpack_take_integer(buf, 7)?;
    let total = consumed.checked_add(len)?;
    if total > buf.len() {
        return None;
    }
    Some(&buf[total..])
}

/// Walks an HPACK header block the same way `hpack::Decoder::decode` does,
/// verifying every representation's integers and string lengths are complete
/// and in-bounds. A `true` result guarantees `decode` cannot hit the
/// `.ok().unwrap()` panic in its size-update path; a `false` result covers
/// exactly the malformed inputs `decode` would reject or panic on. Structural
/// only — it does not validate table indices or Huffman content, which
/// `decode` already reports as ordinary errors.
pub(super) fn hpack_block_is_sound(mut buf: &[u8]) -> bool {
    while let Some(&first) = buf.first() {
        // Representation dispatch matches `hpack`'s `FieldRepresentation::new`.
        let (prefix_bits, is_literal) = if first & 0x80 != 0 {
            (7, false) // Indexed.
        } else if first & 0x40 != 0 {
            (6, true) // Literal, incremental indexing.
        } else if first & 0x20 != 0 {
            (5, false) // Dynamic table size update.
        } else {
            (4, true) // Literal without indexing / never indexed.
        };

        let Some((index, int_consumed)) = hpack_take_integer(buf, prefix_bits) else {
            return false;
        };
        buf = &buf[int_consumed..];

        if is_literal {
            // A zero table index means the name is a literal string; otherwise
            // the name is taken from the table and only the value follows.
            if index == 0 {
                buf = match hpack_skip_string(buf) {
                    Some(rest) => rest,
                    None => return false,
                };
            }
            buf = match hpack_skip_string(buf) {
                Some(rest) => rest,
                None => return false,
            };
        }
    }
    true
}

fn decode_headers_block_with_storage(
    decoder: &mut hpack::Decoder<'static>,
    block: &[u8],
    max_header_bytes: usize,
    store_regular_headers: bool,
    path_cache: Option<&PathInternCache>,
) -> Result<HeaderBlock, Http2ProtocolError> {
    if let Some(headers) =
        decode_fast_literal_headers(block, max_header_bytes, store_regular_headers, path_cache)?
    {
        return Ok(headers);
    }
    // The `hpack` crate `.ok().unwrap()`s a failed integer decode in its
    // dynamic-table-size-update path, so a truncated or over-long
    // size-update integer panics inside `decode` rather than returning
    // `Err`. That input is attacker-controlled and pre-auth. `catch_unwind`
    // only contains it on `panic = "unwind"` builds; a `panic = "abort"`
    // service would abort the process. So reject any block that is not
    // structurally sound *before* the decoder sees it — `hpack_block_is_sound`
    // walks the same representations `hpack` does and fails on exactly the
    // inputs that would make `decode_integer` return `Err`, which is the only
    // panic trigger. `catch_unwind` stays as defense in depth. The decoder is
    // torn down with the connection on this error, so its post-panic state is
    // never reused.
    if !hpack_block_is_sound(block) {
        return Err(Http2ProtocolError::HpackUnsupported);
    }
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decoder.decode(block)))
        .map_err(|_| Http2ProtocolError::HpackUnsupported)?
        .map_err(|_| Http2ProtocolError::HpackUnsupported)?;

    let mut out = HeaderBlock::default();
    for (name, value) in decoded {
        let name = std::str::from_utf8(&name).map_err(|_| Http2ProtocolError::HpackUnsupported)?;
        let value =
            std::str::from_utf8(&value).map_err(|_| Http2ProtocolError::HpackUnsupported)?;
        add_header_with_storage(
            &mut out,
            name,
            value,
            max_header_bytes,
            store_regular_headers,
            path_cache,
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
    path_cache: Option<&PathInternCache>,
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
            path_cache,
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
    add_header_with_storage(out, name, value, max_header_bytes, true, None)
}

fn add_header_with_storage(
    out: &mut HeaderBlock,
    name: &str,
    value: &str,
    max_header_bytes: usize,
    store_regular_headers: bool,
    path_cache: Option<&PathInternCache>,
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
                out.path = Some(own_request_path(path_cache, value));
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
    // Validate the name/value bytes the same way in compact and public modes so
    // both reject identical malformed input. The compact path must not allocate
    // a public `HeaderName` / `HeaderValue` just to validate a header it will
    // not store, so the byte rules live here instead of in `http`'s
    // constructors. Public mode runs the same rules, then builds the typed
    // pieces (which cannot fail after this check) only when it stores them.
    if !is_valid_header_name(name) || !is_valid_header_value(value) {
        return Err(Http2ProtocolError::InvalidPseudoHeaders);
    }
    // Parse the admission/gRPC facts by string. Names are already lowercase
    // (uppercase is rejected above), so a direct match is exact.
    match name {
        "host" => out.host_non_empty = !value.is_empty(),
        "content-type" => out.grpc_content_type = crate::grpc::is_grpc_content_type(value),
        "grpc-encoding" => {
            if !value.eq_ignore_ascii_case("identity") {
                out.grpc_encoding_unsupported = true;
            }
        }
        "content-length" => {
            if out.saw_content_length {
                return Err(Http2ProtocolError::ContentLengthMismatch);
            }
            out.content_length = Some(parse_content_length(value)?);
            out.saw_content_length = true;
        }
        // gRPC response facts: a string parse and (only on errors) one owned
        // message. Requests never carry these, so the server path pays nothing.
        // First value wins, matching `HeaderMap::get` on the public path.
        "grpc-status" => {
            if !out.saw_grpc_status {
                out.saw_grpc_status = true;
                out.grpc_status = value.trim().parse::<u16>().ok();
            }
        }
        "grpc-message" if out.grpc_message.is_none() => {
            out.grpc_message = Some(value.to_owned());
        }
        _ => {}
    }
    if store_regular_headers {
        // Only the storing path pays the public header allocation.
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?;
        let header_value =
            HeaderValue::from_str(value).map_err(|_| Http2ProtocolError::InvalidPseudoHeaders)?;
        #[cfg(test)]
        PUBLIC_HEADER_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
        out.headers.append(header_name, header_value);
    }
    Ok(())
}

/// RFC 9110 token bytes. HTTP/2 header names are already required to be
/// lowercase (uppercase is rejected before this is called), so this mirrors
/// what `http::HeaderName::from_bytes` accepts for the names that reach here.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_token_byte)
}

/// Mirror `http::HeaderValue` byte rules: visible ASCII plus space, horizontal
/// tab, and obs-text (`0x80..=0xFF`); reject other control bytes and DEL.
fn is_valid_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (byte >= b' ' && byte != 0x7f))
}

// Test seam: counts how many public `HeaderName`/`HeaderValue` pairs the decode
// builds. The compact path must leave this at zero for skipped regular headers.
#[cfg(test)]
thread_local! {
    pub(super) static PUBLIC_HEADER_CONSTRUCTIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
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
        let decoded = decode_headers_block_compact_with(&mut decoder, &block, 1024, None).unwrap();
        assert_eq!(decoded.path.as_deref(), Some("/pkg.Service/Unary"));
        assert!(decoded.host_non_empty);
        assert!(decoded.grpc_content_type);
        assert!(decoded.grpc_encoding_unsupported);
        assert_eq!(decoded.content_length, Some(9));
        assert!(decoded.headers.is_empty());
        validate_request_headers(&decoded).unwrap();
    }

    #[test]
    fn compact_and_public_agree_on_duplicate_grpc_status_and_message() {
        // A response carrying duplicate gRPC status/message headers must decode
        // the same on the compact (SubmitGrpcUnary) path and the public
        // (Replied) path. The public path reads HeaderMap::get (first value), so
        // the compact path must also take the first value, not the last.
        let mut block = Vec::new();
        encode_literal_header("grpc-status", "5", &mut block);
        encode_literal_header("grpc-status", "0", &mut block);
        encode_literal_header("grpc-message", "first", &mut block);
        encode_literal_header("grpc-message", "second", &mut block);

        let mut decoder = hpack::Decoder::new();
        let compact = decode_headers_block_compact_with(&mut decoder, &block, 1024, None).unwrap();
        assert_eq!(compact.grpc_status, Some(5));
        assert_eq!(compact.grpc_message.as_deref(), Some("first"));

        let mut decoder = hpack::Decoder::new();
        let public = decode_headers_block_with(&mut decoder, &block, 1024, None).unwrap();
        let compact_status = crate::grpc::grpc_status_from_compact(
            compact.grpc_status.unwrap(),
            compact.grpc_message.as_deref(),
        );
        let public_status = crate::grpc::grpc_status_from_header_map(&public.headers).unwrap();
        assert_eq!(compact_status.code, public_status.code);
        assert_eq!(compact_status.message, public_status.message);
    }

    fn grpc_block_with_extra_metadata(extra: usize) -> Vec<u8> {
        let mut block = Vec::new();
        encode_literal_header(":method", "POST", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":authority", "example.com", &mut block);
        encode_literal_header(":path", "/pkg.Service/Unary", &mut block);
        encode_literal_header("content-type", "application/grpc", &mut block);
        encode_literal_header("te", "trailers", &mut block);
        encode_literal_header("grpc-encoding", "identity", &mut block);
        for i in 0..extra {
            // Custom gRPC metadata headers: ordinary headers a service might see.
            encode_literal_header(&format!("x-meta-{i}"), &format!("value-{i}"), &mut block);
        }
        block
    }

    #[test]
    fn compact_decode_builds_no_public_headers_regardless_of_metadata_count() {
        // The number of ordinary headers must not change the compact path's
        // public-header construction count: it must stay zero. The public path
        // builds exactly one per stored header.
        for extra in [0usize, 4, 16] {
            let block = grpc_block_with_extra_metadata(extra);

            PUBLIC_HEADER_CONSTRUCTIONS.with(|count| count.set(0));
            let mut decoder = hpack::Decoder::new();
            let compact =
                decode_headers_block_compact_with(&mut decoder, &block, 4096, None).unwrap();
            let compact_built = PUBLIC_HEADER_CONSTRUCTIONS.with(|count| count.get());
            assert_eq!(
                compact_built, 0,
                "compact built public headers (extra={extra})"
            );
            assert!(compact.headers.is_empty());
            // Facts still parsed from the same bytes.
            assert!(compact.grpc_content_type);
            assert!(!compact.grpc_encoding_unsupported);
            assert!(compact.authority_non_empty);
            validate_request_headers(&compact).unwrap();

            PUBLIC_HEADER_CONSTRUCTIONS.with(|count| count.set(0));
            let mut decoder = hpack::Decoder::new();
            let public = decode_headers_block_with(&mut decoder, &block, 4096, None).unwrap();
            let public_built = PUBLIC_HEADER_CONSTRUCTIONS.with(|count| count.get());
            // content-type, te, grpc-encoding, plus `extra` custom headers.
            assert_eq!(
                public_built,
                3 + extra,
                "public build count (extra={extra})"
            );
            assert_eq!(public.headers.len(), 3 + extra);
            assert!(public.grpc_content_type);
        }
    }

    fn compact_and_public_agree(block: &[u8]) -> Result<(), Http2ProtocolError> {
        let mut compact_decoder = hpack::Decoder::new();
        let compact = decode_headers_block_compact_with(&mut compact_decoder, block, 256, None)
            .and_then(|h| validate_request_headers(&h).map(|()| h));
        let mut public_decoder = hpack::Decoder::new();
        let public = decode_headers_block_with(&mut public_decoder, block, 256, None)
            .and_then(|h| validate_request_headers(&h).map(|()| h));
        assert_eq!(
            compact.as_ref().map(|_| ()),
            public.as_ref().map(|_| ()),
            "compact/public disagree on accept/reject"
        );
        assert_eq!(
            compact.as_ref().err(),
            public.as_ref().err(),
            "compact/public disagree on error kind"
        );
        compact.map(|_| ())
    }

    #[test]
    fn compact_and_public_reject_identical_malformed_inputs() {
        let base = |extra: &[(&str, &str)]| {
            let mut block = Vec::new();
            encode_literal_header(":method", "POST", &mut block);
            encode_literal_header(":scheme", "http", &mut block);
            encode_literal_header(":authority", "example.com", &mut block);
            encode_literal_header(":path", "/svc/M", &mut block);
            for (name, value) in extra {
                encode_literal_header(name, value, &mut block);
            }
            block
        };

        // Well-formed: both accept.
        assert!(compact_and_public_agree(&base(&[("content-type", "application/grpc")])).is_ok());

        // Uppercase name.
        assert!(compact_and_public_agree(&base(&[("X-Bad", "v")])).is_err());
        // Invalid token byte in name (space).
        assert!(compact_and_public_agree(&base(&[("bad name", "v")])).is_err());
        // Invalid value byte (newline) — must reject before any storage decision.
        assert!(compact_and_public_agree(&base(&[("x-meta", "bad\nvalue")])).is_err());
        // Forbidden connection-control header.
        assert!(compact_and_public_agree(&base(&[("connection", "keep-alive")])).is_err());
        // Invalid `te`.
        assert!(compact_and_public_agree(&base(&[("te", "gzip")])).is_err());
        // Duplicate content-length.
        assert!(
            compact_and_public_agree(&base(&[("content-length", "1"), ("content-length", "1")]))
                .is_err()
        );
        // Invalid content-length value.
        assert!(compact_and_public_agree(&base(&[("content-length", "ten")])).is_err());
    }

    #[test]
    fn compact_and_public_reject_identical_oversized_header_blocks() {
        // Over the byte cap: both paths must fail closed at the same place.
        let big = "a".repeat(512);
        let mut block = Vec::new();
        encode_literal_header(":method", "POST", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":authority", "example.com", &mut block);
        encode_literal_header(":path", "/svc/M", &mut block);
        encode_literal_header("x-big", &big, &mut block);
        assert_eq!(
            compact_and_public_agree(&block),
            Err(Http2ProtocolError::HeadersTooLarge)
        );
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

    fn grpc_request_block(path: &str) -> Vec<u8> {
        let mut block = Vec::new();
        encode_literal_header(":method", "POST", &mut block);
        encode_literal_header(":scheme", "http", &mut block);
        encode_literal_header(":authority", "example.com", &mut block);
        encode_literal_header(":path", path, &mut block);
        encode_literal_header("content-type", "application/grpc", &mut block);
        block
    }

    #[test]
    fn path_cache_lookup_reuses_one_arc_after_remember() {
        let mut cache = PathInternCache::with_capacity(8);
        // A miss before remember; the path is not cached by lookup alone.
        assert!(cache.lookup("/pkg.Service/Unary").is_none());
        let arc: Arc<str> = Arc::from("/pkg.Service/Unary");
        cache.remember(&arc);
        let hit = cache
            .lookup("/pkg.Service/Unary")
            .expect("cached after remember");
        // The warmed call reuses the cached allocation, not a fresh one.
        assert!(Arc::ptr_eq(&arc, &hit));
        assert_eq!(cache.evictions(), 0);
    }

    #[test]
    fn path_cache_evicts_least_recently_used_when_full() {
        let mut cache = PathInternCache::with_capacity(2);
        cache.remember(&Arc::from("/a")); // [a]
        cache.remember(&Arc::from("/b")); // [b, a]
        // Full: the next distinct path evicts the LRU entry (/a), not the new one.
        cache.remember(&Arc::from("/c")); // evict a -> [c, b]
        assert_eq!(cache.evictions(), 1);
        assert!(cache.lookup("/a").is_none());
        assert!(cache.lookup("/b").is_some());
        assert!(cache.lookup("/c").is_some());
        // Re-remembering /b marks it most-recently-used, so the next eviction
        // takes /c instead.
        cache.remember(&Arc::from("/b")); // [b, c]
        cache.remember(&Arc::from("/d")); // evict c -> [d, b]
        assert_eq!(cache.evictions(), 2);
        assert!(cache.lookup("/c").is_none());
        assert!(cache.lookup("/b").is_some());
    }

    #[test]
    fn path_cache_admits_warmed_route_under_path_churn() {
        // The adversarial case: a flood of distinct one-shot paths fills the
        // cache, then a real warmed route arrives. LRU admission must still cache
        // it so the next call reuses it — a full cache cannot lock it out.
        let mut cache = PathInternCache::with_capacity(4);
        for i in 0..4 {
            cache.remember(&Arc::from(format!("/junk{i}").as_str()));
        }
        assert!(cache.lookup("/real").is_none());
        let real: Arc<str> = Arc::from("/real");
        cache.remember(&real);
        let hit = cache
            .lookup("/real")
            .expect("warmed route admitted under churn");
        assert!(Arc::ptr_eq(&real, &hit));
    }

    #[test]
    fn compact_decode_reuses_a_remembered_request_path() {
        let block = grpc_request_block("/pkg.Service/Unary");
        let mut cache = PathInternCache::with_capacity(8);
        let mut decoder = hpack::Decoder::new();
        let first =
            decode_headers_block_compact_with(&mut decoder, &block, 4096, Some(&cache)).unwrap();
        let first_path = first.path.expect("first path");
        // Cache it as the connection does once the request validates.
        cache.remember(&first_path);
        let mut decoder = hpack::Decoder::new();
        let second =
            decode_headers_block_compact_with(&mut decoder, &block, 4096, Some(&cache)).unwrap();
        let second_path = second.path.expect("second path");
        assert_eq!(&*second_path, "/pkg.Service/Unary");
        // The warmed decode reuses the remembered allocation. Fails if the decode
        // rebuilds a fresh owned path per request.
        assert!(Arc::ptr_eq(&first_path, &second_path));
        assert_eq!(cache.evictions(), 0);
    }

    #[test]
    fn decode_does_not_cache_until_remembered() {
        // Decode never inserts, so an unvalidated request cannot poison the
        // cache: two decodes without remember are distinct allocations and the
        // cache stays empty.
        let block = grpc_request_block("/svc/M");
        let cache = PathInternCache::with_capacity(8);
        let mut decoder = hpack::Decoder::new();
        let first = decode_headers_block_compact_with(&mut decoder, &block, 4096, Some(&cache))
            .unwrap()
            .path
            .expect("first path");
        let mut decoder = hpack::Decoder::new();
        let second = decode_headers_block_compact_with(&mut decoder, &block, 4096, Some(&cache))
            .unwrap()
            .path
            .expect("second path");
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(cache.lookup("/svc/M").is_none());
        assert_eq!(cache.evictions(), 0);
    }

    #[test]
    fn decode_without_cache_still_owns_the_path() {
        // The client response path passes no cache; `:path` never appears there,
        // but the fallback must still produce a valid owned path when it does.
        let block = grpc_request_block("/pkg.Service/Unary");
        let mut decoder = hpack::Decoder::new();
        let decoded = decode_headers_block_compact_with(&mut decoder, &block, 4096, None).unwrap();
        assert_eq!(decoded.path.as_deref(), Some("/pkg.Service/Unary"));
    }

    #[test]
    fn truncated_dynamic_table_size_update_is_typed_error_not_panic() {
        // `0x3f` opens a dynamic-table-size-update (prefix-5 value 31 == the
        // prefix max) that promises continuation octets the block does not
        // carry. Un-patched, `hpack` unwraps the failed integer decode and
        // panics; the block is attacker-supplied, so the gated path must
        // surface a protocol error instead. (This exact block panics
        // `hpack::Decoder::decode`; the earlier `2c c1 3f` did not — `0xc1`
        // is an out-of-range index that errors gracefully before the decoder
        // ever reaches the trailing byte.)
        let result = decode_headers_block(&[0x3f], 4096);
        assert!(matches!(result, Err(Http2ProtocolError::HpackUnsupported)));
    }

    #[test]
    fn soundness_walker_rejects_the_size_update_panic_inputs() {
        // The two shapes that make `hpack`'s size-update path unwrap an
        // `Err`: a truncated continuation and a too-long (>5 octet) integer.
        assert!(!hpack_block_is_sound(&[0x2c, 0xc1, 0x3f])); // truncated
        assert!(!hpack_block_is_sound(&[0x3f, 0xff, 0xff, 0xff, 0xff, 0xff])); // too long
        assert!(!hpack_block_is_sound(&[0x3f])); // size-update prefix, no continuation
    }

    #[test]
    fn soundness_walker_accepts_well_formed_blocks() {
        // Indexed field `:method: GET` (static index 2) is a single sound byte.
        assert!(hpack_block_is_sound(&[0x82]));
        assert!(hpack_block_is_sound(&[])); // empty block is trivially sound
        // A real tina-encoded literal block round-trips through the walker.
        let mut block = Vec::new();
        encode_literal_header(":path", "/x", &mut block);
        encode_literal_header("host", "example.com", &mut block);
        assert!(hpack_block_is_sound(&block));
    }

    #[test]
    fn soundness_walker_rejects_truncated_literal_string() {
        // Literal-without-indexing, new name (index 0), name length 4 but only
        // one octet present: the string runs off the end.
        assert!(!hpack_block_is_sound(&[0x00, 0x04, b'a']));
    }

    #[test]
    fn walker_gates_every_short_block_against_the_real_decoder() {
        // Exhaustive differential check over all 1- and 2-byte blocks, run
        // under the test binary's `panic = "unwind"` so a panicking `decode`
        // is observable (the fuzzer's `panic = "abort"` cannot see it). Two
        // directions: the walker must never admit a block that panics `decode`
        // (soundness — the security property), and it must never reject a block
        // `decode` accepts cleanly (completeness — no interop breakage).
        fn decode_outcome(block: &[u8]) -> Result<bool, ()> {
            // Ok(true) = decoded cleanly, Ok(false) = graceful decode error,
            // Err(()) = panicked.
            std::panic::catch_unwind(|| {
                let mut decoder = hpack::Decoder::new();
                decoder.decode(block).is_ok()
            })
            .map_err(|_| ())
        }
        let mut blocks: Vec<Vec<u8>> = (0u16..=255).map(|b| vec![b as u8]).collect();
        for hi in 0u16..=255 {
            for lo in 0u16..=255 {
                blocks.push(vec![hi as u8, lo as u8]);
            }
        }
        for block in blocks {
            let sound = hpack_block_is_sound(&block);
            match decode_outcome(&block) {
                Err(()) => assert!(
                    !sound,
                    "walker admitted a block that panics decode: {block:02x?}"
                ),
                Ok(true) => assert!(
                    sound,
                    "walker rejected a block decode accepts: {block:02x?}"
                ),
                Ok(false) => {} // graceful decode error: walker may accept or reject.
            }
        }
    }
}
