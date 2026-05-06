#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Tina framed request/reply.
//!
//! Not gRPC. Not a general RPC framework. The narrow question this
//! crate answers:
//!
//! > Can Tina model bounded request/reply over a byte stream, with
//! > timeouts and visible overload?
//!
//! Wire format is deliberately boring; semantics lean on the existing
//! Tina runtime — typed isolate addresses, isolate calls with timeout,
//! and the `Full` / `Closed` / `Timeout` outcomes. This crate owns no
//! sockets, schedules no tasks, hides no queues — those concerns stay
//! in [`tina_runtime`](https://docs.rs/tina-runtime).
//!
//! # No public wire compatibility promise
//!
//! Encode/decode is not stable across releases; do not persist frames
//! or expose them to untrusted peers.
//!
//! # Scope
//!
//! - Frame format: length prefix, version, request id, kind, service
//!   name, method name, server-reported error code, opaque payload.
//! - Decode-before-allocate: length prefix is checked against
//!   `max_frame_size` before any body buffer is allocated.
//! - Server-reported error codes only. Client-observed conditions
//!   (`timeout`, `connection_closed`) never appear as wire frames.
//! - Pluggable payload encoding via [`Encoding`]; ships [`Json`].
//!   Encode/decode enforces `max_size`.
//! - Connection isolate ([`Connection`]) per accepted TCP stream:
//!   bounded in-flight, write queue, idle timeout, observable close.
//! - Service registry ([`Registry`]) maps names to
//!   `Address<ServiceCall, ServiceReply>`; forwards via isolate-call.
//! - Client stub ([`Client`]) per outbound TCP stream: bounded
//!   in-flight multiplexing, per-request deadlines, out-of-order
//!   reply matching, visible failure of pending calls on close.
//! - Topology adapters ([`SingleService`], plus [`PooledService`]
//!   and [`ShardedService`] type reservations) — the
//!   `#[tina_rpc::service]` macro emits over them. See [`service`]
//!   for the topology rationale.
//! - Typed dispatch core ([`MethodTable`], [`Dispatch`]) — runtime-free
//!   typed call routing. The `#[tina_rpc::service]` macro emits over
//!   this.
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
mod dispatch;
mod encoding;
mod frame;
mod registry;
pub mod service;

pub use client::{
    Client, ClientConfig, ClientInit, ClientMsg, ClientRequest, ClientResult, ClientResultMsg,
    ClientStream,
};
pub use connection::{
    BadPeerReason, CloseReason, Connection, ConnectionConfig, ConnectionInit, ConnectionMsg,
    RouterReply, RouterRequest,
};
pub use dispatch::{Dispatch, Method, MethodTable, PayloadLimits};
pub use encoding::{Encoding, EncodingError, EncodingErrorKind, Json};
pub use frame::{
    DecodeError, EncodeError, FRAME_VERSION_V1, Frame, FrameError, FrameKind, FrameLimits,
    LENGTH_PREFIX_SIZE, MAX_METHOD_LEN, MAX_SERVICE_LEN, decode, decode_body, encode,
    parse_length_prefix,
};
pub use registry::{
    Registry, RegistryBuilder, RegistryConfig, RegistryMsg, ServiceCall, ServiceReply,
};
pub use service::{PooledService, ServiceConfig, ServiceHandler, ShardedService, SingleService};
pub use tina_rpc_macros::service;
