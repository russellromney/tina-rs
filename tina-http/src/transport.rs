//! HTTP/1.1 over TCP or TLS. The transport rail keeps `TcpRead` /
//! `TlsRead` distinct in the trace; match arms stay exhaustive, no
//! trait-object indirection.

use std::time::Duration;

use tina_runtime::{
    StreamId, TlsStreamId, TypedCall, tcp_close_stream, tcp_read, tcp_write, tls_close, tls_read,
    tls_write,
};

/// Sentinel for `tls_lane_deadline` on TCP-only call sites — TCP
/// has no per-call deadline today.
pub const TLS_DEADLINE_UNUSED: Duration = Duration::ZERO;

/// Per-stream transport rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpTransport {
    Tcp(StreamId),
    Tls(TlsStreamId),
}

impl HttpTransport {
    /// Read up to `max_len` plaintext bytes. `tls_lane_deadline`
    /// applies only on the TLS branch — pass [`TLS_DEADLINE_UNUSED`]
    /// when the transport is known to be TCP.
    pub fn read_call(&self, max_len: usize, tls_lane_deadline: Duration) -> TypedCall<Vec<u8>> {
        match self {
            Self::Tcp(stream) => tcp_read(*stream, max_len),
            Self::Tls(stream) => tls_read(*stream, max_len, tls_lane_deadline),
        }
    }

    /// Write `bytes`. `tls_lane_deadline` applies only on the TLS branch.
    pub fn write_call(&self, bytes: Vec<u8>, tls_lane_deadline: Duration) -> TypedCall<usize> {
        match self {
            Self::Tcp(stream) => tcp_write(*stream, bytes),
            Self::Tls(stream) => tls_write(*stream, bytes, tls_lane_deadline),
        }
    }

    /// Close the stream. `tls_lane_deadline` applies only on the TLS branch.
    pub fn close_call(&self, tls_lane_deadline: Duration) -> TypedCall<()> {
        match self {
            Self::Tcp(stream) => tcp_close_stream(*stream),
            Self::Tls(stream) => tls_close(*stream, tls_lane_deadline),
        }
    }
}
