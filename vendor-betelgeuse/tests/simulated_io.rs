#![feature(allocator_api)]

use std::{
    alloc::Global,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use betelgeuse::{
    AcceptCompletion, IO, IOLoop, RecvCompletion, SendCompletion,
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

#[test]
fn simulated_accept_recv_send_roundtrip() -> io::Result<()> {
    let io = SimulatedIO::new();
    let loop_handle = io.loop_handle(Global);
    let listener = loop_handle.io().socket()?;
    listener.bind(localhost(0))?;
    let bound = listener.local_addr()?;

    let mut accept = AcceptCompletion::new();
    listener.accept(&mut accept)?;
    assert!(!io.step()?);
    assert!(!accept.has_result());

    let peer = io.connect(bound, b"ping".to_vec())?;
    pump_until(&io, || accept.has_result())?;
    let accepted = accept.take_result().unwrap()?;

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
fn simulated_send_respects_partial_write_limit() -> io::Result<()> {
    let io = SimulatedIO::with_config(SimulatedConfig {
        max_send_chunk: Some(2),
        ..SimulatedConfig::default()
    });
    let loop_handle = io.loop_handle(Global);
    let listener = loop_handle.io().socket()?;
    listener.bind(localhost(0))?;
    let bound = listener.local_addr()?;

    let mut accept = AcceptCompletion::new();
    listener.accept(&mut accept)?;
    let peer = io.connect(bound, Vec::new())?;
    pump_until(&io, || accept.has_result())?;
    let accepted = accept.take_result().unwrap()?;

    let mut send = SendCompletion::new();
    accepted.send(&mut send, b"abcdef".to_vec())?;
    pump_until(&io, || send.has_result())?;
    assert_eq!(send.take_result().unwrap()?, 2);
    assert_eq!(peer.output(), b"ab");
    Ok(())
}

#[test]
fn simulated_delay_defers_ready_completion() -> io::Result<()> {
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
