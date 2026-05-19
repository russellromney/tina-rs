//! UDP rail constructors.

use std::net::SocketAddr;

use super::{CallInput, CallOutput, TypedCall, UdpSocketId};

/// Returns a typed UDP bind helper.
pub fn udp_bind(addr: SocketAddr) -> TypedCall<(UdpSocketId, SocketAddr)> {
    TypedCall::new(CallInput::UdpBind { addr }, CallOutput::into_udp_bound)
}

/// Returns a typed UDP send helper.
pub fn udp_send_to(socket: UdpSocketId, peer: SocketAddr, bytes: Vec<u8>) -> TypedCall<usize> {
    TypedCall::new(
        CallInput::UdpSendTo {
            socket,
            peer,
            bytes,
        },
        CallOutput::into_udp_sent,
    )
}

/// Returns a typed UDP receive helper.
pub fn udp_recv_from(
    socket: UdpSocketId,
    max_len: usize,
) -> TypedCall<(SocketAddr, Vec<u8>, bool)> {
    TypedCall::new(
        CallInput::UdpRecvFrom { socket, max_len },
        CallOutput::into_udp_received,
    )
}

/// Returns a typed UDP close helper.
pub fn udp_close_socket(socket: UdpSocketId) -> TypedCall<()> {
    TypedCall::new(
        CallInput::UdpSocketClose { socket },
        CallOutput::into_udp_socket_closed,
    )
}
