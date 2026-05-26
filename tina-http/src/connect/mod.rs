//! Production-shaped outbound connect: unresolved endpoints, a bounded
//! connect policy, a Tina-shaped Happy-Eyeballs connect helper, and typed
//! reports.
//!
//! The existing resolved targets ([`crate::HttpTarget`],
//! [`crate::Http2Target`], [`crate::WebSocketTarget`], [`crate::GrpcTarget`])
//! stay as low-level escape hatches: they take one resolved
//! [`std::net::SocketAddr`]. This module adds the production shape above
//! them — a user says "connect to this host" and gets bounded DNS, bounded
//! connect attempts, clear TLS/SNI/authority truth, and a [`ConnectReport`]
//! when it fails.
//!
//! Layers:
//!
//! - [`endpoint`] — unresolved [`HttpEndpoint`], [`Http2Endpoint`],
//!   [`GrpcEndpoint`], [`WebSocketEndpoint`], plus [`EndpointId`],
//!   [`EndpointGeneration`], [`ConnectSecurity`], [`ResolvedEndpoint`].
//! - [`policy`] — [`ConnectPolicy`] over runtime DNS + TCP/TLS connect,
//!   with [`AddressFamilyPolicy`] and [`HappyEyeballsPolicy`], validation,
//!   and budget surfaces.
//! - [`report`] — [`ConnectReport`], [`ConnectAttemptReport`], and the DNS /
//!   attempt outcome vocabulary.
//! - [`attempts`] — the [`ConnectAttempts`] helper that admits a bounded set
//!   of attempts, races them via a [`tina_runtime::CallGroup`], cancels
//!   losers, tombstones late completions, and builds the [`ConnectReport`].
//!   The connect attempt itself is a `call_cancelable` to a protocol client
//!   isolate (a [`crate::WebSocketClientConnection`], an
//!   [`crate::Http2ClientConnection`]) that owns its own stream and closes
//!   it on stop — there is no separate connector isolate to leak.

pub mod attempts;
pub mod endpoint;
pub mod policy;
pub mod report;
pub mod websocket_manager;

pub use attempts::{
    AttemptKey, ConnectAttempts, ConnectAttemptsError, ConnectStep, DnsClassification,
};
pub use websocket_manager::{
    InstallError, RetainedSessionReport, SessionEndReason, WebSocketClientManager,
    WebSocketConnectOutcome, WebSocketManagerAddr, WebSocketManagerConfig,
    WebSocketManagerConfigError, WebSocketManagerHandles, WebSocketManagerMsg,
    WebSocketManagerReply, WebSocketManagerReport, WebSocketManagerShutdownReport,
    WebSocketManagerState, WebSocketSessionError, WsConnAddr, build_websocket_client_manager,
};
pub use endpoint::{
    ConnectSecurity, EndpointGeneration, EndpointId, GrpcEndpoint, Http2Endpoint, HttpEndpoint,
    ResolvedEndpoint, WebSocketEndpoint,
};
pub use policy::{
    AddressFamilyPolicy, ConnectPolicy, ConnectPolicyError, HappyEyeballsPolicy,
};
pub use report::{
    AddressFamily, ConnectAttemptOutcome, ConnectAttemptReport, ConnectReport, ConnectTlsTruth,
    DnsOutcome,
};
