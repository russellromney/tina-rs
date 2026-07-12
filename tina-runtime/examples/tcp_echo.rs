//! Runnable TCP echo demonstration.
//!
//! A listener isolate is the supervised parent, each accepted connection
//! becomes a restartable child handler isolate, and the runtime-owned call
//! contract drives bind/accept/read/write/close.
//!
//! The `ThreadedRuntime` worker thread performs the I/O; `main` is just a
//! client. It starts the runtime, registers the listener, connects three
//! times, checks each echo, and shuts down. No manual `step()` pumping and no
//! hand-rolled mailbox.
//!
//! Run with:
//! ```bash
//! cargo run -p tina-runtime --example tcp_echo
//! ```
//!
//! Binds to `127.0.0.1:0`; the listener publishes the actual bound address
//! through its shared slot. A deterministic trace-shape proof lives in the
//! `#[test]` at the bottom, which uses the explicit-step runtime.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tina::{RestartBudget, RestartPolicy, prelude::*};
use tina_runtime::{
    DefaultThreadedMailboxFactory, ListenerId, StreamId, ThreadedRuntime, tcp_accept, tcp_bind,
    tcp_close_listener, tcp_close_stream, tcp_read, tcp_write,
};
use tina_supervisor::SupervisorConfig;

type BoundAddr = Arc<Mutex<Option<SocketAddr>>>;

#[derive(Debug, Clone)]
enum ConnectionEvent {
    Begin,
    Read(Vec<u8>),
    Wrote(usize),
    Closed,
    IoFailed,
}

#[derive(Debug)]
struct Connection {
    stream: StreamId,
    max_chunk: usize,
    pending_write: Vec<u8>,
}

#[tina_runtime::isolate(message = ConnectionEvent)]
impl Connection {
    fn handle(
        &mut self,
        msg: ConnectionEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ConnectionEvent::Begin => read_call(self.stream, self.max_chunk),
            ConnectionEvent::Read(bytes) => {
                if bytes.is_empty() {
                    close_call(self.stream)
                } else {
                    self.pending_write = bytes;
                    write_call(self.stream, self.pending_write.clone())
                }
            }
            ConnectionEvent::Wrote(count) => {
                if count >= self.pending_write.len() {
                    self.pending_write.clear();
                    read_call(self.stream, self.max_chunk)
                } else {
                    self.pending_write.drain(..count);
                    write_call(self.stream, self.pending_write.clone())
                }
            }
            ConnectionEvent::Closed | ConnectionEvent::IoFailed => stop(),
        }
    }
}

fn read_call(stream: StreamId, max_len: usize) -> Effect<Connection> {
    tcp_read(stream, max_len).then(|result| match result {
        Ok(bytes) => ConnectionEvent::Read(bytes),
        Err(_) => ConnectionEvent::IoFailed,
    })
}

fn write_call(stream: StreamId, bytes: Vec<u8>) -> Effect<Connection> {
    tcp_write(stream, bytes).then(|result| match result {
        Ok(count) => ConnectionEvent::Wrote(count),
        Err(_) => ConnectionEvent::IoFailed,
    })
}

fn close_call(stream: StreamId) -> Effect<Connection> {
    tcp_close_stream(stream).then(|result| match result {
        Ok(()) => ConnectionEvent::Closed,
        Err(_) => ConnectionEvent::IoFailed,
    })
}

#[derive(Debug, Clone)]
enum ListenerEvent {
    Start,
    Bound {
        listener: ListenerId,
        addr: SocketAddr,
    },
    AcceptNext,
    Close,
    Closed,
    Accepted {
        stream: StreamId,
    },
    IoFailed,
}

#[derive(Debug)]
struct Listener {
    bind_addr: SocketAddr,
    bound_addr_slot: BoundAddr,
    max_chunk: usize,
    target_accepts: usize,
    accepted_count: usize,
    listener: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerEvent,
    send = Outbound<ListenerEvent>,
    spawn = RestartableChildDefinition<Connection>,
)]
impl Listener {
    fn handle(
        &mut self,
        msg: ListenerEvent,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ListenerEvent::Start => {
                let addr = self.bind_addr;
                tcp_bind(addr).then(move |result| match result {
                    Ok((listener, local_addr)) => ListenerEvent::Bound {
                        listener,
                        addr: local_addr,
                    },
                    Err(_) => ListenerEvent::IoFailed,
                })
            }
            ListenerEvent::Bound { listener, addr } => {
                self.listener = Some(listener);
                *self
                    .bound_addr_slot
                    .lock()
                    .expect("bound addr mutex never poisoned") = Some(addr);
                tcp_accept(listener).then(|result| match result {
                    Ok((stream, _peer_addr)) => ListenerEvent::Accepted { stream },
                    Err(_) => ListenerEvent::IoFailed,
                })
            }
            ListenerEvent::AcceptNext => {
                let listener = self.listener.expect("listener stored before re-arm");
                tcp_accept(listener).then(|result| match result {
                    Ok((stream, _peer_addr)) => ListenerEvent::Accepted { stream },
                    Err(_) => ListenerEvent::IoFailed,
                })
            }
            ListenerEvent::Accepted { stream } => {
                self.accepted_count += 1;
                let max_chunk = self.max_chunk;
                let child = spawn(
                    RestartableChildDefinition::new(
                        move || Connection {
                            stream,
                            max_chunk,
                            pending_write: Vec::new(),
                        },
                        8,
                    )
                    .with_initial_message(|| ConnectionEvent::Begin),
                );
                let follow_up = if self.accepted_count < self.target_accepts {
                    ListenerEvent::AcceptNext
                } else {
                    ListenerEvent::Close
                };
                batch([child, ctx.send_self(follow_up)])
            }
            ListenerEvent::Close => {
                let listener = self.listener.expect("listener stored before close");
                tcp_close_listener(listener).then(|result| match result {
                    Ok(()) => ListenerEvent::Closed,
                    Err(_) => ListenerEvent::IoFailed,
                })
            }
            ListenerEvent::Closed => stop(),
            ListenerEvent::IoFailed => stop(),
        }
    }
}

/// Connects, sends `payload`, half-closes, and reads the echo until EOF.
fn echo_roundtrip(addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.write_all(payload).expect("write");
    stream.shutdown(Shutdown::Write).expect("write shutdown");
    let mut received = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(error) => panic!("client read error: {error}"),
        }
    }
    received
}

/// Polls `probe` until it yields a value or the timeout elapses.
fn wait_for<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() > deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn main() {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound: BoundAddr = Arc::new(Mutex::new(None));

    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);

    let payloads = vec![
        b"hello from the tina-rs echo example".to_vec(),
        b"listener re-armed for a second client".to_vec(),
        b"third client proves graceful shutdown".to_vec(),
    ];

    let listener = runtime
        .register_with_capacity::<Listener, ListenerEvent>(
            Listener {
                bind_addr,
                bound_addr_slot: Arc::clone(&bound),
                max_chunk: 256,
                target_accepts: payloads.len(),
                accepted_count: 0,
                listener: None,
            },
            8,
        )
        .expect("register listener");

    runtime
        .supervise(
            listener,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
        )
        .expect("supervise listener");

    runtime
        .try_send(listener, ListenerEvent::Start)
        .expect("start listener");

    let local_addr = wait_for(Duration::from_secs(2), || {
        *bound.lock().expect("bound lock")
    })
    .expect("listener published its bound address");
    println!("listening on {local_addr}");

    // Register the stop waiter before the listener can close itself.
    let listener_stopped = runtime
        .observe_isolate_complete(listener)
        .expect("register listener completion observer");

    let echoed: Vec<Vec<u8>> = payloads
        .iter()
        .map(|payload| echo_roundtrip(local_addr, payload))
        .collect();
    assert_eq!(echoed, payloads, "echoed bytes must match sent payloads");

    // The listener closes itself after the last accept.
    listener_stopped
        .wait(Duration::from_secs(5))
        .expect("listener stops cleanly");

    println!(
        "echoed {} payloads through the runtime call contract",
        echoed.len()
    );

    runtime.shutdown().expect("clean shutdown");
}

#[cfg(test)]
mod tests {
    use super::*;

    use tina_runtime::{CallKind, DefaultMailboxFactory, Runtime, RuntimeEvent, RuntimeEventKind};

    fn count_call_completed(trace: &[RuntimeEvent], kind: CallKind) -> usize {
        trace
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == kind
                )
            })
            .count()
    }

    fn count_spawned(trace: &[RuntimeEvent]) -> usize {
        trace
            .iter()
            .filter(|event| matches!(event.kind(), RuntimeEventKind::Spawned { .. }))
            .count()
    }

    fn step_until<F>(
        runtime: &mut Runtime<SingleShard, DefaultMailboxFactory>,
        timeout: Duration,
        label: &str,
        predicate: F,
    ) where
        F: Fn(&Runtime<SingleShard, DefaultMailboxFactory>) -> bool,
    {
        let deadline = Instant::now() + timeout;
        while !predicate(runtime) {
            if Instant::now() > deadline {
                panic!("step_until({label}): predicate not satisfied within timeout");
            }
            runtime.step();
            thread::sleep(Duration::from_millis(2));
        }
    }

    // The explicit-step runtime is the semantic oracle: it makes the trace
    // shape deterministic, so this is where the bind/accept/spawn/stop counts
    // are asserted. `main` proves the live behavior; this proves the shape.
    #[test]
    fn trace_shows_bind_accept_spawn_and_graceful_stop() {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
        let bound: BoundAddr = Arc::new(Mutex::new(None));
        let payload = b"deterministic echo proof".to_vec();

        let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
        let listener = runtime.register_with_capacity::<Listener, ListenerEvent>(
            Listener {
                bind_addr,
                bound_addr_slot: Arc::clone(&bound),
                max_chunk: 256,
                target_accepts: 1,
                accepted_count: 0,
                listener: None,
            },
            8,
        );
        runtime.supervise(
            listener,
            SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
        );
        runtime
            .try_send(listener, ListenerEvent::Start)
            .expect("start listener");

        step_until(&mut runtime, Duration::from_secs(2), "bind", |_| {
            bound.lock().expect("bound lock").is_some()
        });
        let local_addr = bound.lock().expect("bound lock").expect("bound addr");

        let handle = thread::spawn(move || echo_roundtrip(local_addr, &payload));

        step_until(
            &mut runtime,
            Duration::from_secs(5),
            "listener stop",
            |runtime| {
                !runtime.has_in_flight_calls()
                    && runtime.trace().iter().any(|event| {
                        event.isolate() == listener.isolate()
                            && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
                    })
            },
        );

        let echoed = handle.join().expect("client thread");
        assert_eq!(echoed, b"deterministic echo proof");

        let trace = runtime.trace();
        assert_eq!(count_call_completed(trace, CallKind::TcpBind), 1);
        assert_eq!(count_call_completed(trace, CallKind::TcpAccept), 1);
        assert_eq!(count_call_completed(trace, CallKind::TcpWrite), 1);
        assert_eq!(count_call_completed(trace, CallKind::TcpListenerClose), 1);
        assert_eq!(count_spawned(trace), 1);
        assert_eq!(
            trace
                .iter()
                .filter(|event| {
                    event.isolate() == listener.isolate()
                        && matches!(event.kind(), RuntimeEventKind::IsolateStopped)
                })
                .count(),
            1
        );
    }
}
