#![feature(allocator_api)]

use std::{
    alloc::Global,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use betelgeuse::{
    AcceptCompletion, ConnectCompletion, IO, IOLoop, MkdirCompletion, OpenOptions, RecvCompletion,
    SendCompletion,
    io::simulated::{SimulatedConfig, SimulatedDelay, SimulatedIO},
};

fn localhost(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn pump_until<F>(io: &SimulatedIO, mut done: F) -> io::Result<()>
where
    F: FnMut() -> bool,
{
    for _ in 0..32 {
        if done() {
            return Ok(());
        }
        io.step()?;
    }
    panic!("simulated I/O operation did not complete");
}

fn connected_stream(
    io: &SimulatedIO,
    input: &[u8],
) -> io::Result<(
    Box<dyn betelgeuse::IOSocket>,
    betelgeuse::io::simulated::SimulatedPeer,
)> {
    let loop_handle = io.loop_handle(Global);
    let listener = loop_handle.io().socket()?;
    listener.bind(localhost(0))?;
    let bound = listener.local_addr()?;

    let mut accept = AcceptCompletion::new();
    listener.accept(&mut accept)?;
    let peer = io.connect(bound, input.to_vec())?;
    pump_until(io, || accept.has_result())?;
    Ok((accept.take_result().unwrap()?, peer))
}

#[test]
fn simulated_tcp_accept_recv_send_roundtrip() -> io::Result<()> {
    let io = SimulatedIO::new();
    let (accepted, peer) = connected_stream(&io, b"ping")?;

    let mut recv = RecvCompletion::new();
    accepted.recv(&mut recv, 32)?;
    pump_until(&io, || recv.has_result())?;
    assert_eq!(recv.take_result().unwrap()?, b"ping");

    let mut send = SendCompletion::new();
    accepted.send(&mut send, b"pong".to_vec())?;
    pump_until(&io, || send.has_result())?;
    assert_eq!(send.take_result().unwrap()?, 4);
    assert_eq!(peer.output(), b"pong");
    Ok(())
}

#[test]
fn simulated_tcp_connect_accept_send_recv_roundtrip() -> io::Result<()> {
    let io = SimulatedIO::new();
    let loop_handle = io.loop_handle(Global);
    let listener = loop_handle.io().socket()?;
    listener.bind(localhost(0))?;
    let bound = listener.local_addr()?;
    let client = loop_handle.io().socket()?;

    let mut connect = ConnectCompletion::new();
    client.connect(&mut connect, bound)?;
    let mut accept = AcceptCompletion::new();
    listener.accept(&mut accept)?;
    pump_until(&io, || connect.has_result() && accept.has_result())?;
    connect.take_result().unwrap()?;
    let server = accept.take_result().unwrap()?;

    let mut client_send = SendCompletion::new();
    client.send(&mut client_send, b"ping".to_vec())?;
    let mut server_recv = RecvCompletion::new();
    server.recv(&mut server_recv, 4)?;
    pump_until(&io, || client_send.has_result() && server_recv.has_result())?;
    assert_eq!(client_send.take_result().unwrap()?, 4);
    assert_eq!(&server_recv.take_result().unwrap()?, b"ping");

    let mut server_send = SendCompletion::new();
    server.send(&mut server_send, b"pong".to_vec())?;
    let mut client_recv = RecvCompletion::new();
    client.recv(&mut client_recv, 4)?;
    pump_until(&io, || server_send.has_result() && client_recv.has_result())?;
    assert_eq!(server_send.take_result().unwrap()?, 4);
    assert_eq!(&client_recv.take_result().unwrap()?, b"pong");
    Ok(())
}

#[test]
fn simulated_tcp_send_respects_partial_write_limit() -> io::Result<()> {
    let io = SimulatedIO::with_config(SimulatedConfig {
        max_send_chunk: Some(2),
        ..SimulatedConfig::default()
    });
    let (accepted, peer) = connected_stream(&io, &[])?;

    let mut send = SendCompletion::new();
    accepted.send(&mut send, b"abcdef".to_vec())?;
    pump_until(&io, || send.has_result())?;
    assert_eq!(send.take_result().unwrap()?, 2);
    assert_eq!(peer.output(), b"ab");
    Ok(())
}

#[test]
fn simulated_tcp_delay_defers_ready_completion() -> io::Result<()> {
    let io = SimulatedIO::with_config(SimulatedConfig {
        seed: 7,
        completion_delay: SimulatedDelay::Every {
            one_in: 1,
            steps: 1,
        },
        max_send_chunk: None,
    });
    let loop_handle = io.loop_handle(Global);
    let listener = loop_handle.io().socket()?;
    listener.bind(localhost(0))?;
    let bound = listener.local_addr()?;

    let mut accept = AcceptCompletion::new();
    listener.accept(&mut accept)?;
    let _peer = io.connect(bound, Vec::new())?;

    assert!(io.step()?);
    assert!(!accept.has_result());
    assert!(io.step()?);
    assert!(accept.has_result());
    Ok(())
}

#[test]
fn simulated_tcp_recv_waits_until_peer_provides_input_or_eof() -> io::Result<()> {
    let io = SimulatedIO::new();
    let (accepted, peer) = connected_stream(&io, &[])?;
    peer.push_input(b"later");

    let mut recv = RecvCompletion::new();
    accepted.recv(&mut recv, 32)?;
    pump_until(&io, || recv.has_result())?;
    assert_eq!(recv.take_result().unwrap()?, b"later");

    let mut eof = RecvCompletion::new();
    peer.close_input();
    accepted.recv(&mut eof, 32)?;
    pump_until(&io, || eof.has_result())?;
    assert!(eof.take_result().unwrap()?.is_empty());
    Ok(())
}

#[test]
fn simulated_tcp_reports_listener_and_stream_addresses() -> io::Result<()> {
    let io = SimulatedIO::new();
    let loop_handle = io.loop_handle(Global);
    let listener = loop_handle.io().socket()?;
    listener.bind(localhost(0))?;
    let bound = listener.local_addr()?;
    assert_eq!(bound.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_ne!(bound.port(), 0);

    let mut accept = AcceptCompletion::new();
    listener.accept(&mut accept)?;
    let _peer = io.connect(bound, b"hello".to_vec())?;
    pump_until(&io, || accept.has_result())?;
    let accepted = accept.take_result().unwrap()?;

    assert_eq!(accepted.local_addr()?, bound);
    assert_eq!(accepted.peer_addr()?.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    Ok(())
}

#[test]
fn simulated_tcp_backend_keeps_file_operations_unsupported() {
    let io = SimulatedIO::new();
    let open_error = match io.open(std::path::Path::new("not-modeled"), OpenOptions::default()) {
        Ok(_) => panic!("simulated backend should not open files"),
        Err(error) => error,
    };
    assert_eq!(open_error.kind(), io::ErrorKind::Unsupported);

    let mut mkdir = MkdirCompletion::new();
    let mkdir_error = io
        .mkdir(&mut mkdir, std::path::Path::new("not-modeled"), 0o755)
        .unwrap_err();
    assert_eq!(mkdir_error.kind(), io::ErrorKind::Unsupported);
}
