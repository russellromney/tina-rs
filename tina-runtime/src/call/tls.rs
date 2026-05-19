//! TLS rail constructors.

use std::net::SocketAddr;
use std::time::Duration;

use super::{CallInput, CallOutput, TlsListenerId, TlsStreamId, TypedCall};

/// Returns a typed TLS connect helper with explicit DER root certificates.
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
            timeout,
        },
        CallOutput::into_tls_connected,
    )
}

/// Returns a typed TLS server bind helper with explicit DER cert/key.
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
        },
        CallOutput::into_tls_bound,
    )
}

/// Returns a typed TLS server accept helper.
pub fn tls_accept(
    listener: TlsListenerId,
    timeout: Duration,
) -> TypedCall<(TlsStreamId, SocketAddr)> {
    TypedCall::new(
        CallInput::TlsAccept { listener, timeout },
        CallOutput::into_tls_accepted,
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

/// Returns a typed TLS close helper.
pub fn tls_close(stream: TlsStreamId, timeout: Duration) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TlsClose { stream, timeout },
        CallOutput::into_tls_closed,
    )
}
