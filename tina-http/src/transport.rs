//! Transport rail: HTTP over TCP or TLS.
//!
//! HTTP/1.1 framing does not care if bytes came from TCP or TLS. The
//! [`HttpTransport`] enum is the narrow carrier that distinguishes the
//! two at the runtime-call boundary so the trace stays honest:
//! `TcpRead` vs `TlsRead`, `TcpWrite` vs `TlsWrite`. Match arms remain
//! exhaustive — there is no trait-object indirection.
//!
//! [`HttpListenerTransport`] is the listener-side analogue used by
//! `HttpsListener`; the plain TCP listener stays typed as
//! `ListenerId` since it never crosses transports.
//!
//! Read/write/close helpers on `HttpTransport` build a `TypedCall<T>`
//! the issuing isolate can `.reply(...)` into a continuation. The TLS
//! lane requires a per-call timeout; TCP does not. The helpers accept
//! a `tls_timeout` parameter that is ignored on the TCP branch.

use std::time::Duration;

use tina_runtime::{
    ListenerId, StreamId, TlsListenerId, TlsStreamId, TypedCall, tcp_close_stream, tcp_read,
    tcp_write, tls_close, tls_read, tls_write,
};

/// Per-stream transport rail. Used by the outbound `HttpClient` to
/// dispatch one call's reads/writes/close to the right runtime lane,
/// and by `HttpsConnection` to keep its server-side state typed
/// distinct from `HttpConnection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpTransport {
    /// Plaintext TCP stream.
    Tcp(StreamId),
    /// TLS stream over the runtime TLS lane.
    Tls(TlsStreamId),
}

/// Per-listener transport rail. `HttpListener` keeps `ListenerId`
/// directly; `HttpsListener` keeps `TlsListenerId`. The enum exists so
/// shared code paths (trace inspection, shutdown reports) can talk
/// about both without losing type information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpListenerTransport {
    /// Plaintext TCP listener.
    Tcp(ListenerId),
    /// TLS listener over the runtime TLS lane.
    Tls(TlsListenerId),
}

impl HttpTransport {
    /// Builds a transport-appropriate read call. Both branches reply
    /// with `Vec<u8>` plaintext bytes; the trace event distinguishes
    /// `TcpRead` from `TlsRead`. `tls_timeout` is ignored on the TCP
    /// branch — TCP reads have no per-call deadline today.
    pub fn read_call(&self, max_len: usize, tls_timeout: Duration) -> TypedCall<Vec<u8>> {
        match self {
            Self::Tcp(stream) => tcp_read(*stream, max_len),
            Self::Tls(stream) => tls_read(*stream, max_len, tls_timeout),
        }
    }

    /// Builds a transport-appropriate write call. Both branches reply
    /// with `usize` bytes accepted by the lane.
    pub fn write_call(&self, bytes: Vec<u8>, tls_timeout: Duration) -> TypedCall<usize> {
        match self {
            Self::Tcp(stream) => tcp_write(*stream, bytes),
            Self::Tls(stream) => tls_write(*stream, bytes, tls_timeout),
        }
    }

    /// Builds a transport-appropriate close call. Both branches reply
    /// with `()`.
    pub fn close_call(&self, tls_timeout: Duration) -> TypedCall<()> {
        match self {
            Self::Tcp(stream) => tcp_close_stream(*stream),
            Self::Tls(stream) => tls_close(*stream, tls_timeout),
        }
    }
}
