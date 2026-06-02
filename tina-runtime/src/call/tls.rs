//! TLS rail constructors.

use std::net::SocketAddr;
use std::time::Duration;

use super::{
    CallInput, CallOutput, TlsListenerId, TlsReadBufReply, TlsStreamId, TlsWriteOwnedReply,
    TypedCall,
};

/// Returns a typed TLS connect helper with explicit DER root certificates
/// and no ALPN. Existing (HTTP/1) callers stay on this signature.
pub fn tls_connect(
    addr: SocketAddr,
    server_name: impl Into<String>,
    root_certificates: Vec<Vec<u8>>,
    timeout: Duration,
) -> TypedCall<TlsStreamId> {
    TypedCall::new(
        CallInput::TlsConnect {
            addr,
            server_name: server_name.into(),
            root_certificates,
            alpn_protocols: Vec::new(),
            timeout,
        },
        CallOutput::into_tls_connected,
    )
}

/// Returns a typed TLS connect helper that offers ALPN protocols and
/// reports the negotiated protocol. `alpn_protocols` are raw wire bytes
/// in preference order (e.g. `vec![b"h2".to_vec()]`); an empty list is
/// equivalent to [`tls_connect`]. When ALPN is offered and the peer
/// negotiates none, the connect fails with
/// [`crate::CallError::TlsAlpnMismatch`].
pub fn tls_connect_alpn(
    addr: SocketAddr,
    server_name: impl Into<String>,
    root_certificates: Vec<Vec<u8>>,
    alpn_protocols: Vec<Vec<u8>>,
    timeout: Duration,
) -> TypedCall<(TlsStreamId, Option<Vec<u8>>)> {
    TypedCall::new(
        CallInput::TlsConnect {
            addr,
            server_name: server_name.into(),
            root_certificates,
            alpn_protocols,
            timeout,
        },
        CallOutput::into_tls_connected_alpn,
    )
}

/// Returns a typed TLS server bind helper with explicit DER cert/key and
/// no ALPN.
pub fn tls_bind(
    addr: SocketAddr,
    certificate_chain: Vec<Vec<u8>>,
    private_key: Vec<u8>,
) -> TypedCall<(TlsListenerId, SocketAddr)> {
    TypedCall::new(
        CallInput::TlsBind {
            addr,
            certificate_chain,
            private_key,
            alpn_protocols: Vec::new(),
        },
        CallOutput::into_tls_bound,
    )
}

/// Returns a typed TLS server bind helper that advertises ALPN protocols.
pub fn tls_bind_alpn(
    addr: SocketAddr,
    certificate_chain: Vec<Vec<u8>>,
    private_key: Vec<u8>,
    alpn_protocols: Vec<Vec<u8>>,
) -> TypedCall<(TlsListenerId, SocketAddr)> {
    TypedCall::new(
        CallInput::TlsBind {
            addr,
            certificate_chain,
            private_key,
            alpn_protocols,
        },
        CallOutput::into_tls_bound,
    )
}

/// Returns a typed TLS server accept helper (stream + peer address).
pub fn tls_accept(
    listener: TlsListenerId,
    timeout: Duration,
) -> TypedCall<(TlsStreamId, SocketAddr)> {
    TypedCall::new(
        CallInput::TlsAccept { listener, timeout },
        CallOutput::into_tls_accepted,
    )
}

/// Returns a typed TLS server accept helper that also reports the ALPN
/// protocol negotiated with the client.
pub fn tls_accept_alpn(
    listener: TlsListenerId,
    timeout: Duration,
) -> TypedCall<(TlsStreamId, SocketAddr, Option<Vec<u8>>)> {
    TypedCall::new(
        CallInput::TlsAccept { listener, timeout },
        CallOutput::into_tls_accepted_alpn,
    )
}

/// Returns a typed TLS listener close helper.
pub fn tls_close_listener(listener: TlsListenerId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TlsListenerClose { listener },
        CallOutput::into_tls_listener_closed,
    )
}

/// Returns a typed TLS read helper.
pub fn tls_read(stream: TlsStreamId, max_len: usize, timeout: Duration) -> TypedCall<Vec<u8>> {
    TypedCall::new(
        CallInput::TlsRead {
            stream,
            max_len,
            timeout,
        },
        CallOutput::into_tls_read,
    )
}

/// Returns a typed TLS read helper that reuses caller-owned plaintext storage.
pub fn tls_read_buf(
    stream: TlsStreamId,
    buffer: Vec<u8>,
    max_len: usize,
    timeout: Duration,
) -> TypedCall<TlsReadBufReply> {
    TypedCall::new(
        CallInput::TlsReadBuf {
            stream,
            buffer,
            max_len,
            timeout,
        },
        CallOutput::into_tls_read_buf,
    )
}

/// Returns a typed TLS write helper.
pub fn tls_write(stream: TlsStreamId, bytes: Vec<u8>, timeout: Duration) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::TlsWrite {
            stream,
            bytes,
            timeout,
        },
        CallOutput::into_tls_wrote,
    )
}

/// Returns a typed TLS write helper that gives the caller-owned bytes back.
pub fn tls_write_owned(
    stream: TlsStreamId,
    bytes: Vec<u8>,
    timeout: Duration,
) -> TypedCall<TlsWriteOwnedReply> {
    TypedCall::new(
        CallInput::TlsWriteOwned {
            stream,
            bytes,
            timeout,
        },
        CallOutput::into_tls_wrote_owned,
    )
}

/// Returns a typed TLS close helper.
pub fn tls_close(stream: TlsStreamId, timeout: Duration) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TlsClose { stream, timeout },
        CallOutput::into_tls_closed,
    )
}
