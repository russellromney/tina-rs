//! Native HTTP/2 for Tina: prior-knowledge cleartext h2c, a bounded
//! server isolate, and the typed protocol vocabulary shared with the
//! native client.
//!
//! The frame, HPACK, and protocol-error helpers (`frame`, `headers`,
//! `errors`) are internal to this module tree and are not re-exported
//! on the crate's public API — they are shared between the server and
//! client implementations only.

mod client;
mod errors;
mod frame;
mod headers;
mod server;
mod target;

pub use client::{
    Http2ClientConnection, Http2ClientGrpcUnaryRequest, Http2ClientLimits, Http2ClientMsg,
    Http2ClientOutcome, Http2ClientReply, Http2ClientReport, Http2ClientRequest,
    Http2ClientRequestBody, Http2ClientResponse, Http2ClientStreamCall,
    Http2ClientStreamingRequest, Http2ResponseChunk,
};
pub use errors::Http2ProtocolError;
pub use server::{
    Http2Connection, Http2ConnectionMsg, Http2ConnectionReply, Http2ConnectionReport, Http2Limits,
    Http2Listener, Http2ListenerMsg, Http2Outcome, Http2RequestParts, Http2ServerConfig,
    Http2ServiceMessage, Http2StreamReport, Http2StreamState,
};
pub use target::{AlpnProtocols, Http2Target};

/// Pure decode entry points for the out-of-workspace fuzz harness.
///
/// `frame` and `headers` internals are `pub(super)` on purpose; this
/// module is the only sanctioned way past that boundary and exists only
/// under the `fuzzing` feature.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing {
    use super::errors::Http2ProtocolError;

    /// Drives the REAL production header-decode entry
    /// ([`headers::decode_headers_block`]), not a private copy of the gate. This
    /// is what closes the "decorative gate" hole: under the fuzzer's
    /// `panic = "abort"`, deleting the `hpack_block_is_sound` gate makes a panic
    /// input reach `hpack::Decoder::decode`, panic, and abort the process — a
    /// fuzzer-visible crash. Driving production also exercises the fast-literal
    /// path (`decode_fast_literal_headers` → `decode_plain_hpack_string`), which
    /// runs first on every inbound block and no other target reaches. A generous
    /// header cap keeps the byte-limit branch off the panic-containment path.
    pub fn fuzz_hpack_guarded_decode(block: &[u8]) {
        const FUZZ_MAX_HEADER_BYTES: usize = 1 << 20;
        let _ = super::headers::decode_headers_block(block, FUZZ_MAX_HEADER_BYTES);
    }

    /// Strips DATA/HEADERS frame padding (and HEADERS priority bytes) the way
    /// the read loops do, over arbitrary flags+payload bytes. The harness
    /// asserts neither view panics.
    pub fn fuzz_payload_views(flags: u8, payload: &[u8]) {
        let _ = super::frame::data_payload_view(flags, payload);
        let _ = super::frame::headers_payload_view(flags, payload);
    }

    /// Decodes one frame header + length check, as the read loop does.
    pub fn fuzz_decode_frame_meta(
        buffer: &[u8],
        max_frame_size: usize,
    ) -> Result<Option<(u8, u8, u32, usize)>, Http2ProtocolError> {
        Ok(super::frame::try_decode_frame_meta(buffer, max_frame_size)?
            .map(|meta| (meta.ty, meta.flags, meta.stream_id, meta.total)))
    }
}
