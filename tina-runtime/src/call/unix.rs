//! Unix-domain socket rail constructors.
//!
//! These mirror the TCP rail surface (bind/accept/connect/read/write/close)
//! but speak filesystem paths instead of `SocketAddr` and use the distinct
//! [`UnixListenerId`] / [`UnixStreamId`] resource types so wrong-rail
//! operations surface as typed `InvalidResource` errors instead of
//! accidental TCP success.
//!
//! Lifecycle truth:
//!
//! - The runtime owns the path. Closing the listener removes the
//!   underlying socket file.
//! - `Unsupported` is the typed answer on platforms without a Unix-
//!   domain backend.

use std::path::PathBuf;

use super::{
    CallInput, CallOutput, TypedCall, UnixListenerId, UnixStreamId, WriteOwnedError,
    WriteOwnedReply,
};

/// Returns a typed Unix bind helper.
pub fn unix_bind(path: impl Into<PathBuf>) -> TypedCall<(UnixListenerId, PathBuf)> {
    TypedCall::new(
        CallInput::UnixBind { path: path.into() },
        CallOutput::into_unix_bound,
    )
}

/// Returns a typed Unix accept helper.
pub fn unix_accept(listener: UnixListenerId) -> TypedCall<UnixStreamId> {
    TypedCall::new(
        CallInput::UnixAccept { listener },
        CallOutput::into_unix_accepted,
    )
}

/// Returns a typed Unix connect helper.
pub fn unix_connect(path: impl Into<PathBuf>) -> TypedCall<UnixStreamId> {
    TypedCall::new(
        CallInput::UnixConnect { path: path.into() },
        CallOutput::into_unix_connected,
    )
}

/// Returns a typed Unix read helper.
pub fn unix_read(stream: UnixStreamId, max_len: usize) -> TypedCall<Vec<u8>> {
    TypedCall::new(
        CallInput::UnixRead { stream, max_len },
        CallOutput::into_unix_read,
    )
}

/// Returns a typed Unix write helper.
pub fn unix_write(stream: UnixStreamId, bytes: Vec<u8>) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::UnixWrite { stream, bytes },
        CallOutput::into_unix_wrote,
    )
}

/// Returns a Unix write helper that gives the bytes back.
pub fn unix_write_owned(
    stream: UnixStreamId,
    bytes: Vec<u8>,
) -> TypedCall<WriteOwnedReply, WriteOwnedError> {
    TypedCall::new(
        CallInput::UnixWriteOwned {
            stream,
            bytes,
            start: 0,
        },
        CallOutput::into_unix_wrote_owned,
    )
}

pub(crate) fn unix_write_owned_from(
    stream: UnixStreamId,
    bytes: Vec<u8>,
    start: usize,
) -> TypedCall<WriteOwnedReply, WriteOwnedError> {
    TypedCall::new(
        CallInput::UnixWriteOwned {
            stream,
            bytes,
            start,
        },
        CallOutput::into_unix_wrote_owned,
    )
}

/// Returns a typed Unix listener close helper.
pub fn unix_close_listener(listener: UnixListenerId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::UnixListenerClose { listener },
        CallOutput::into_unix_listener_closed,
    )
}

/// Returns a typed Unix stream close helper.
pub fn unix_close_stream(stream: UnixStreamId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::UnixStreamClose { stream },
        CallOutput::into_unix_stream_closed,
    )
}
