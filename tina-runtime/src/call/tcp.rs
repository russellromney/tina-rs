//! TCP rail constructors.
//!
//! Thin wrappers around [`TypedCall`] for runtime-owned TCP operations.
//! Re-exported from the crate root.

use std::net::SocketAddr;

use super::{CallInput, CallOutput, ListenerId, StreamId, TypedCall};

/// Returns a typed TCP bind helper that later yields one listener id and
/// bound address.
pub fn tcp_bind(addr: SocketAddr) -> TypedCall<(ListenerId, SocketAddr)> {
    TypedCall::new(CallInput::TcpBind { addr }, CallOutput::into_tcp_bound)
}

/// Returns a typed TCP accept helper that later yields one stream id and peer
/// address.
pub fn tcp_accept(listener: ListenerId) -> TypedCall<(StreamId, SocketAddr)> {
    TypedCall::new(
        CallInput::TcpAccept { listener },
        CallOutput::into_tcp_accepted,
    )
}

/// Returns a typed TCP connect helper that later yields one stream id, local
/// address, and peer address.
pub fn tcp_connect(addr: SocketAddr) -> TypedCall<(StreamId, SocketAddr, SocketAddr)> {
    TypedCall::new(
        CallInput::TcpConnect { addr },
        CallOutput::into_tcp_connected,
    )
}

/// Returns a typed TCP read helper that later yields the bytes read.
pub fn tcp_read(stream: StreamId, max_len: usize) -> TypedCall<Vec<u8>> {
    TypedCall::new(
        CallInput::TcpRead { stream, max_len },
        CallOutput::into_tcp_read,
    )
}

/// Returns a typed TCP write helper that later yields the accepted byte count.
pub fn tcp_write(stream: StreamId, bytes: Vec<u8>) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::TcpWrite { stream, bytes },
        CallOutput::into_tcp_wrote,
    )
}

/// Returns a typed listener-close helper that later yields `Result<(), CallError>`.
pub fn tcp_close_listener(listener: ListenerId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TcpListenerClose { listener },
        CallOutput::into_tcp_listener_closed,
    )
}

/// Returns a typed stream-close helper that later yields `Result<(), CallError>`.
pub fn tcp_close_stream(stream: StreamId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::TcpStreamClose { stream },
        CallOutput::into_tcp_stream_closed,
    )
}
