//! TCP rail constructors.
//!
//! Thin wrappers around [`TypedCall`] for runtime-owned TCP operations.
//! Re-exported from the crate root.

use std::net::SocketAddr;

use super::{
    CallInput, CallOutput, ListenerId, StreamId, TcpReadBufError, TcpReadBufReply,
    TcpWriteOwnedCloseReply, TcpWriteOwnedError, TcpWriteOwnedReply, TypedCall,
};

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

/// Returns a typed TCP read helper that reuses caller-owned storage.
pub fn tcp_read_buf(
    stream: StreamId,
    buffer: Vec<u8>,
    max_len: usize,
) -> TypedCall<TcpReadBufReply, TcpReadBufError> {
    TypedCall::new(
        CallInput::TcpReadBuf {
            stream,
            buffer,
            max_len,
        },
        CallOutput::into_tcp_read_buf,
    )
}

/// Returns a typed TCP write helper that later yields the accepted byte count.
pub fn tcp_write(stream: StreamId, bytes: Vec<u8>) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::TcpWrite { stream, bytes },
        CallOutput::into_tcp_wrote,
    )
}

/// Returns a typed TCP write helper that gives the caller-owned bytes back.
pub fn tcp_write_owned(
    stream: StreamId,
    bytes: Vec<u8>,
) -> TypedCall<TcpWriteOwnedReply, TcpWriteOwnedError> {
    TypedCall::new(
        CallInput::TcpWriteOwned {
            stream,
            bytes,
            start: 0,
        },
        CallOutput::into_tcp_wrote_owned,
    )
}

pub(crate) fn tcp_write_owned_from(
    stream: StreamId,
    bytes: Vec<u8>,
    start: usize,
) -> TypedCall<TcpWriteOwnedReply, TcpWriteOwnedError> {
    TypedCall::new(
        CallInput::TcpWriteOwned {
            stream,
            bytes,
            start,
        },
        CallOutput::into_tcp_wrote_owned,
    )
}

/// Returns a typed TCP terminal-write helper.
///
/// If the runtime accepts the full buffer, it closes the stream in the same
/// backend operation and returns `closed == true`. Partial writes leave the
/// stream open and return `closed == false` so callers can retry the remaining
/// bytes without truncating the wire.
pub fn tcp_write_owned_close(
    stream: StreamId,
    bytes: Vec<u8>,
) -> TypedCall<TcpWriteOwnedCloseReply, TcpWriteOwnedError> {
    TypedCall::new(
        CallInput::TcpWriteOwnedClose { stream, bytes },
        CallOutput::into_tcp_wrote_owned_close,
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
