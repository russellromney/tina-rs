#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Tina framed request/reply, first form (phase 052).
//!
//! This crate is a probe, not a product. It is **not gRPC** and **not a general
//! RPC framework**. It answers one narrow question:
//!
//! > Can Tina model bounded request/reply over a byte stream, with timeouts and
//! > visible overload?
//!
//! The wire format is deliberately boring. The semantics lean on the existing
//! Tina runtime: typed isolate addresses, isolate calls with timeout, and the
//! `Full` / `Closed` / `Timeout` outcomes. This crate does not own sockets,
//! schedule tasks, or hide queues — those concerns stay in
//! [`tina_runtime`](https://docs.rs/tina-runtime).
//!
//! # No public wire compatibility promise
//!
//! First form may change. Encode/decode functions are stable for one phase
//! only; do not persist frames or expose them to untrusted peers.
//!
//! # First-form scope
//!
//! - Frame format with length prefix, version, request id, kind, service name,
//!   method name, server-reported error code, and opaque payload bytes.
//! - Decode-before-allocate: the length prefix is checked against
//!   `max_frame_size` before any body buffer is allocated.
//! - Server-reported error codes only. Client-observed conditions (`timeout`,
//!   `connection_closed`) never appear as wire frames.
//! - Pluggable payload encoding via [`Encoding`]. First impl: [`Json`].
//!   Encode/decode enforces `max_size` before invoking the underlying
//!   serializer.
//!
//! - Connection isolate ([`Connection`]) per accepted TCP stream, with
//!   bounded in-flight, write queue, idle timeout, and observable close
//!   paths.
//! - Service registry ([`Registry`]) that maps service names to uniform
//!   `Address<ServiceCall, ServiceReply>` and forwards via isolate-call.
//! - Client stub ([`Client`]) per outbound TCP stream, with bounded
//!   in-flight multiplexing, per-request deadlines, out-of-order reply
//!   matching, and visible failure of pending calls on close.
//!
//! Out of scope: simulation (Rock 6), Eiffel comparison (Rock 7).
//!
//! # Example
//!
//! ```
//! use tina_rpc::{decode, encode, Frame, FrameError, FrameKind, FrameLimits};
//!
//! let limits = FrameLimits::default();
//!
//! let frame = Frame::request(
//!     /* request_id */ 42,
//!     /* service */ "billing",
//!     /* method */ "charge",
//!     /* payload */ b"{\"cents\": 1000}".to_vec(),
//! );
//!
//! let bytes = encode(&frame, &limits).expect("frame fits in limits");
//! let decoded = decode(&bytes, &limits).expect("encode/decode roundtrip");
//!
//! assert_eq!(decoded.kind, FrameKind::Request);
//! assert_eq!(decoded.request_id, 42);
//! assert_eq!(decoded.service, "billing");
//! assert_eq!(decoded.method, "charge");
//! assert_eq!(decoded.error, None);
//! assert_eq!(decoded.payload, b"{\"cents\": 1000}");
//!
//! let reply = Frame::error(
//!     decoded.request_id,
//!     decoded.service.clone(),
//!     decoded.method.clone(),
//!     FrameError::Full,
//!     Vec::new(),
//! );
//! let reply_bytes = encode(&reply, &limits).expect("error frame fits");
//! let decoded_reply = decode(&reply_bytes, &limits).expect("error roundtrip");
//! assert_eq!(decoded_reply.kind, FrameKind::Error);
//! assert_eq!(decoded_reply.error, Some(FrameError::Full));
//! ```

mod client;
mod connection;
mod encoding;
mod frame;
mod registry;

pub use client::{
    Client, ClientConfig, ClientInit, ClientMsg, ClientRequest, ClientResult, ClientResultMsg,
    ClientStream,
};
pub use connection::{
    BadPeerReason, CloseReason, Connection, ConnectionConfig, ConnectionInit, ConnectionMsg,
    RouterReply, RouterRequest,
};
pub use encoding::{Encoding, EncodingError, EncodingErrorKind, Json};
pub use registry::{Registry, RegistryConfig, RegistryMsg, ServiceCall, ServiceReply};
pub use frame::{
    DecodeError, EncodeError, Frame, FrameError, FrameKind, FrameLimits, FRAME_VERSION_V1,
    LENGTH_PREFIX_SIZE, MAX_METHOD_LEN, MAX_SERVICE_LEN, decode, decode_body, encode,
    parse_length_prefix,
};
