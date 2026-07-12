//! `UnixWriteAll` event-only authoring is identical live and in simulation.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallCompletionRejectedReason, CallError, CallKind, DefaultMailboxFactory, LoopStep, Runtime,
    RuntimeEventKind, UnixAcceptReply, UnixBindReply, UnixListenerCloseReply, UnixListenerId,
    UnixReadReply, UnixStreamCloseReply, UnixStreamId, UnixWriteAll, UnixWriteOwnedReply,
    unix_accept, unix_bind, unix_close_listener, unix_close_stream, unix_connect, unix_read,
};
use tina_sim::{Simulator, SimulatorConfig};

#[derive(Debug, Default)]
struct UnixServiceShard;

impl Shard for UnixServiceShard {
    fn id(&self) -> ShardId {
        ShardId::new(124)
    }
}

#[derive(Debug)]
enum ServerEvent {
    Start,
    Bound(UnixBindReply),
    Accepted(UnixAcceptReply),
    Read(UnixReadReply),
    StreamClosed(UnixStreamCloseReply),
    ListenerClosed(UnixListenerCloseReply),
}

struct Server {
    path: PathBuf,
    listener: Option<UnixListenerId>,
    stream: Option<UnixStreamId>,
    received: Arc<Mutex<Vec<u8>>>,
    close_on_accept: bool,
    read: bool,
}

#[tina_runtime::isolate(event = ServerEvent, shard = UnixServiceShard)]
impl Server {
    fn handle_event(
        &mut self,
        event: ServerEvent,
        _ctx: &mut Context<'_, UnixServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            ServerEvent::Start => {
                unix_bind(self.path.clone()).then_service_event(ServerEvent::Bound)
            }
            ServerEvent::Bound(Ok((listener, _))) => {
                self.listener = Some(listener);
                unix_accept(listener).then_service_event(ServerEvent::Accepted)
            }
            ServerEvent::Accepted(Ok(stream)) => {
                self.stream = Some(stream);
                if self.close_on_accept {
                    self.close_stream()
                } else if self.read {
                    unix_read(stream, 2).then_service_event(ServerEvent::Read)
                } else {
                    noop()
                }
            }
            ServerEvent::Read(Ok(bytes)) if bytes.is_empty() => self.close_stream(),
            ServerEvent::Read(Ok(bytes)) => {
                self.received.lock().unwrap().extend_from_slice(&bytes);
                unix_read(self.stream.expect("accepted stream"), 2)
                    .then_service_event(ServerEvent::Read)
            }
            ServerEvent::StreamClosed(Ok(())) => {
                let listener = self.listener.take().expect("bound listener");
                unix_close_listener(listener).then_service_event(ServerEvent::ListenerClosed)
            }
            ServerEvent::ListenerClosed(Ok(())) => stop(),
            ServerEvent::Bound(Err(error))
            | ServerEvent::Accepted(Err(error))
            | ServerEvent::Read(Err(error))
            | ServerEvent::StreamClosed(Err(error))
            | ServerEvent::ListenerClosed(Err(error)) => {
                panic!("server Unix rail failed: {error:?}")
            }
        }
    }
}

impl Server {
    fn close_stream(&mut self) -> Effect<Self> {
        let stream = self.stream.take().expect("accepted stream");
        unix_close_stream(stream).then_service_event(ServerEvent::StreamClosed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WriterReport {
    outcome: Result<usize, CallError>,
    allocation_preserved: bool,
    write_completions: usize,
}

#[derive(Debug)]
enum WriterEvent {
    Start,
    Connected(Result<UnixStreamId, CallError>),
    Wrote(UnixWriteOwnedReply),
    Closed(UnixStreamCloseReply),
    Stop,
}

struct Writer {
    path: PathBuf,
    payload: Vec<u8>,
    stream: Option<UnixStreamId>,
    write_all: Option<UnixWriteAll>,
    allocation: Option<usize>,
    allocation_preserved: bool,
    write_completions: usize,
    report: Arc<Mutex<Option<WriterReport>>>,
    write_armed: Arc<AtomicBool>,
}

#[tina_runtime::isolate(event = WriterEvent, shard = UnixServiceShard)]
impl Writer {
    fn handle_event(
        &mut self,
        event: WriterEvent,
        _ctx: &mut Context<'_, UnixServiceShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            WriterEvent::Start => {
                unix_connect(self.path.clone()).then_service_event(WriterEvent::Connected)
            }
            WriterEvent::Connected(Ok(stream)) => {
                self.stream = Some(stream);
                let bytes = std::mem::take(&mut self.payload);
                self.allocation = Some(bytes.as_ptr() as usize);
                let mut write_all = UnixWriteAll::new(stream, bytes);
                let effect = write_all
                    .next_service_event(WriterEvent::Wrote)
                    .expect("test payload is non-empty");
                self.write_all = Some(write_all);
                effect
            }
            WriterEvent::Wrote(reply) => {
                self.write_completions += 1;
                let returned = match &reply {
                    Ok(reply) => reply.bytes.as_ptr() as usize,
                    Err(error) => error.bytes.as_ptr() as usize,
                };
                self.allocation_preserved &= self.allocation == Some(returned);
                let write_all = self.write_all.as_mut().expect("write helper armed");
                match write_all.advance_service_event(reply, WriterEvent::Wrote) {
                    LoopStep::Pending(effect) => {
                        self.write_armed.store(true, Ordering::Release);
                        effect
                    }
                    LoopStep::Done(written) => self.finish(Ok(written)),
                    LoopStep::Failed(error) => self.finish(Err(error)),
                }
            }
            WriterEvent::Connected(Err(error)) => self.finish(Err(error)),
            WriterEvent::Closed(Ok(())) | WriterEvent::Stop => stop(),
            WriterEvent::Closed(Err(error)) => panic!("writer close failed: {error:?}"),
        }
    }
}

impl Writer {
    fn finish(&mut self, outcome: Result<usize, CallError>) -> Effect<Self> {
        *self.report.lock().unwrap() = Some(WriterReport {
            outcome,
            allocation_preserved: self.allocation_preserved,
            write_completions: self.write_completions,
        });
        self.write_all = None;
        let Some(stream) = self.stream.take() else {
            return stop();
        };
        unix_close_stream(stream).then_service_event(WriterEvent::Closed)
    }
}

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn socket_path(label: &str) -> PathBuf {
    PathBuf::from(format!(
        "/tmp/tina-unix-write-service-{}-{label}-{}.sock",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
    ))
}

fn writer(
    path: PathBuf,
    payload: Vec<u8>,
) -> (Writer, Arc<Mutex<Option<WriterReport>>>, Arc<AtomicBool>) {
    let report = Arc::new(Mutex::new(None));
    let write_armed = Arc::new(AtomicBool::new(false));
    (
        Writer {
            path,
            payload,
            stream: None,
            write_all: None,
            allocation: None,
            allocation_preserved: true,
            write_completions: 0,
            report: Arc::clone(&report),
            write_armed: Arc::clone(&write_armed),
        },
        report,
        write_armed,
    )
}

#[test]
fn live_runtime_and_simulator_share_event_only_write_all_authoring() {
    let payload = b"event-only-unix-write".to_vec();

    let live_path = socket_path("live");
    let live_received = Arc::new(Mutex::new(Vec::new()));
    let (live_writer, live_report, _) = writer(live_path.clone(), payload.clone());
    let mut runtime = Runtime::new(UnixServiceShard, DefaultMailboxFactory);
    let server = runtime.register_event_service::<Server, ServerEvent, Infallible>(
        Server {
            path: live_path.clone(),
            listener: None,
            stream: None,
            received: Arc::clone(&live_received),
            close_on_accept: false,
            read: true,
        },
        16,
    );
    let client = runtime.register_event_service::<Writer, WriterEvent, Infallible>(live_writer, 16);
    assert!(runtime.try_send_event(server, ServerEvent::Start).is_ok());
    assert!(runtime.try_send_event(client, WriterEvent::Start).is_ok());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while live_report.lock().unwrap().is_none() && std::time::Instant::now() < deadline {
        runtime.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    while runtime.step() > 0 {}
    let live_report = live_report.lock().unwrap().clone().expect("live report");
    assert_eq!(live_report.outcome, Ok(payload.len()));
    assert!(live_report.allocation_preserved);
    assert!(live_report.write_completions >= 1);
    assert_eq!(*live_received.lock().unwrap(), payload);
    drop(runtime);
    let _ = std::fs::remove_file(live_path);

    let sim_path = socket_path("sim");
    let sim_received = Arc::new(Mutex::new(Vec::new()));
    let (sim_writer, sim_report, _) = writer(sim_path.clone(), payload.clone());
    let mut config = SimulatorConfig::default();
    config.unix.default_inbound_capacity = 2;
    config.unix.default_write_cap = 2;
    let mut sim = Simulator::new(UnixServiceShard, config);
    let server = sim.register_event_service(
        Server {
            path: sim_path,
            listener: None,
            stream: None,
            received: Arc::clone(&sim_received),
            close_on_accept: false,
            read: true,
        },
        16,
    );
    let client = sim.register_event_service(sim_writer, 16);
    assert!(sim.try_send_event(server, ServerEvent::Start).is_ok());
    assert!(sim.try_send_event(client, WriterEvent::Start).is_ok());
    sim.run_until_quiescent();
    let sim_report = sim_report.lock().unwrap().clone().expect("sim report");
    assert_eq!(sim_report.outcome, Ok(payload.len()));
    assert!(sim_report.allocation_preserved);
    assert_eq!(sim_report.write_completions, payload.len().div_ceil(2));
    assert_eq!(*sim_received.lock().unwrap(), payload);
}

#[test]
fn peer_close_returns_typed_error_and_original_allocation() {
    let path = socket_path("closed");
    let received = Arc::new(Mutex::new(Vec::new()));
    let (writer, report, _) = writer(path.clone(), b"closed-peer".to_vec());
    let mut sim = Simulator::new(UnixServiceShard, SimulatorConfig::default());
    let server = sim.register_event_service(
        Server {
            path,
            listener: None,
            stream: None,
            received,
            close_on_accept: true,
            read: false,
        },
        8,
    );
    let client = sim.register_event_service(writer, 8);
    assert!(sim.try_send_event(server, ServerEvent::Start).is_ok());
    assert!(sim.try_send_event(client, WriterEvent::Start).is_ok());
    sim.run_until_quiescent();

    assert_eq!(
        report.lock().unwrap().as_ref(),
        Some(&WriterReport {
            outcome: Err(CallError::Io),
            allocation_preserved: true,
            write_completions: 1,
        })
    );
}

#[test]
fn owner_stop_cancels_pending_write_without_lingering_authority() {
    let path = socket_path("owner-stop");
    let received = Arc::new(Mutex::new(Vec::new()));
    let (writer, report, armed) = writer(path.clone(), b"write-remains-pending".to_vec());
    let mut config = SimulatorConfig::default();
    config.unix.default_inbound_capacity = 1;
    config.unix.default_write_cap = 1;
    let mut sim = Simulator::new(UnixServiceShard, config);
    let server = sim.register_event_service(
        Server {
            path,
            listener: None,
            stream: None,
            received,
            close_on_accept: false,
            read: false,
        },
        8,
    );
    let client = sim.register_event_service(writer, 8);
    assert!(sim.try_send_event(server, ServerEvent::Start).is_ok());
    assert!(sim.try_send_event(client, WriterEvent::Start).is_ok());
    while !armed.load(Ordering::Acquire) {
        assert!(sim.step() > 0, "writer must arm its owned write");
    }
    assert!(sim.try_send_event(client, WriterEvent::Stop).is_ok());
    sim.run_until_quiescent();

    assert!(report.lock().unwrap().is_none());
    assert!(!sim.has_in_flight_calls());
    assert!(
        sim.trace().iter().any(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::CallCompletionRejected {
                    call_kind: CallKind::UnixWrite,
                    reason: CallCompletionRejectedReason::RequesterClosed,
                    ..
                }
            )
        }),
        "trace: {:#?}",
        sim.trace()
    );
}
