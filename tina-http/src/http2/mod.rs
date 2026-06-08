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
