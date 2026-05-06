//! End-to-end smoke test for the native HTTP/1.1 server.
//!
//! Spins up a `ThreadedRuntime` with one shard, registers a `Counter`
//! service isolate that handles `GET /counter` and `POST /counter`, plus
//! an `HttpListener` that binds to loopback. A scripted client thread
//! talks real TCP to the server and asserts the responses.
//!
//! This test exercises:
//!
//! - `tcp_bind` -> `tcp_accept` loop in the listener;
//! - `tcp_read` -> `parse_request_head` -> service `call` ->
//!   `tcp_write` -> `tcp_close_stream` in the connection isolate;
//! - typed `CallOutcome` translation into status codes (covered for the
//!   happy path; bad-input and overload tests live in dedicated test
//!   files under this `tests/` directory).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use http::{Method, StatusCode};
use tina::{Mailbox, TrySendError, prelude::*};
use tina_http::{
    HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse,
};
use tina_runtime::{MailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig};

#[derive(Debug, Default)]
struct TestShard;

impl Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(101)
    }
}

struct TestMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> TestMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for TestMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut q = self.queue.borrow_mut();
        if q.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        q.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct TestMailboxFactory;

impl MailboxFactory for TestMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(TestMailbox::new(capacity))
    }
}

#[derive(Debug, Default)]
struct Counter {
    value: u64,
}

impl Isolate for Counter {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: TestShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, TestShard>,
    ) -> Effect<Self> {
        let response = match (request.method, request.path.as_str()) {
            (Method::GET, "/counter") => HttpResponse::text(self.value.to_string()),
            (Method::POST, "/counter") => {
                self.value += 1;
                HttpResponse::text(self.value.to_string())
            }
            _ => HttpResponse::with_status(StatusCode::NOT_FOUND),
        };
        reply(response)
    }
}

#[test]
fn native_http_server_serves_get_and_post() {
    let runtime = ThreadedRuntime::with_config(
        TestShard,
        TestMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 32,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let counter = runtime
        .register_with_capacity::<Counter, Infallible>(Counter::default(), 16)
        .expect("register counter");

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("loopback parse");
    let bound_addr_slot = Arc::new(Mutex::new(None));

    let listener_isolate = HttpListener::<TestShard>::new(
        bind_addr,
        Arc::clone(&bound_addr_slot),
        counter,
        HttpLimits::default(),
        Duration::from_secs(2),
        16,
    );
    let listener = runtime
        .register_with_capacity::<HttpListener<TestShard>, _>(listener_isolate, 8)
        .expect("register listener");

    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .expect("send Start");

    let server_addr = wait_for_bound_addr(&bound_addr_slot, Duration::from_secs(2));

    // Three requests in sequence (single-request-per-connection in 048a).
    let response_a = scripted_request(server_addr, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    let response_b = scripted_request(
        server_addr,
        b"POST /counter HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    );
    let response_c = scripted_request(server_addr, b"GET /counter HTTP/1.1\r\nHost: x\r\n\r\n");
    let response_d = scripted_request(server_addr, b"GET /missing HTTP/1.1\r\nHost: x\r\n\r\n");

    runtime
        .try_send(listener, HttpListenerMsg::Stop)
        .expect("send Stop");
    let _ = runtime.shutdown().expect("runtime shutdown");

    assert_status_and_body(&response_a, "200", "0");
    assert_status_and_body(&response_b, "200", "1");
    assert_status_and_body(&response_c, "200", "1");
    assert_status_starts_with(&response_d, "404");
}

fn wait_for_bound_addr(
    slot: &Arc<Mutex<Option<SocketAddr>>>,
    timeout: Duration,
) -> SocketAddr {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(addr) = *slot.lock().expect("bound addr lock") {
            return addr;
        }
        if Instant::now() > deadline {
            panic!("listener bind timed out");
        }
        thread::yield_now();
    }
}

fn scripted_request(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .expect("connect to native server");
    stream.set_read_timeout(Some(Duration::from_secs(2))).expect("read timeout");
    stream.write_all(request).expect("write request");
    stream.flush().expect("flush");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    response
}

fn assert_status_and_body(response: &[u8], expected_status: &str, expected_body: &str) {
    let text = std::str::from_utf8(response).expect("utf8 response");
    assert!(
        text.starts_with(&format!("HTTP/1.1 {expected_status}")),
        "expected status {expected_status} in response: {text:?}"
    );
    assert!(
        text.ends_with(expected_body),
        "expected body {expected_body:?} at end of response: {text:?}"
    );
}

fn assert_status_starts_with(response: &[u8], expected_status: &str) {
    let text = std::str::from_utf8(response).expect("utf8 response");
    assert!(
        text.starts_with(&format!("HTTP/1.1 {expected_status}")),
        "expected status {expected_status} in response: {text:?}"
    );
}
