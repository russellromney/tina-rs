//! Deterministic-simulator parity for paced buffered and streamed HTTP/2 responses.
//!
//! The live wire test proves a peer can grant credit only after consuming
//! response bytes. This companion drives the same server state machine through
//! scripted TCP, including short writes, and pins both trace and peer-visible
//! byte determinism while requiring flow-control pressure to stay observable.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use tina::prelude::*;
use tina_http::{
    Http2Limits, Http2Listener, Http2ListenerMsg, Http2ServerConfig, HttpRequest, HttpResponse,
    IterBodySource, ResponseChunkMsg, ResponseChunkReply,
};
use tina_runtime::{ProtocolFact, RuntimeEventKind, RuntimeFact, stable_trace_hash};
use tina_sim::{
    ObservedPeerOutput, ScriptedListenerConfig, ScriptedPeerConfig, ScriptedTcpConfig, Simulator,
    SimulatorConfig,
};

const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_DATA: u8 = 0x0;
const FRAME_HEADERS: u8 = 0x1;
const FRAME_SETTINGS: u8 = 0x4;
const FRAME_WINDOW_UPDATE: u8 = 0x8;
const FLAG_END_STREAM: u8 = 0x1;
const FLAG_END_HEADERS: u8 = 0x4;
const BODY_LEN: usize = 96 * 1024;

#[derive(Debug, Default)]
struct SimShard;

impl Shard for SimShard {
    fn id(&self) -> ShardId {
        ShardId::new(126)
    }
}

struct LargeBufferedService {
    body: Arc<[u8]>,
}

struct LargeStreamingService {
    source: Address<ResponseChunkMsg, ResponseChunkReply>,
}

impl Isolate for LargeBufferedService {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: tina_runtime::RuntimeCall<HttpRequest>,
        shard: SimShard,
    }

    fn handle(
        &mut self,
        _request: HttpRequest,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.response())
    }

    fn handle_call(
        &mut self,
        _request: HttpRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(self.response())
    }
}

impl LargeBufferedService {
    fn response(&self) -> HttpResponse {
        HttpResponse::with_shared_body(http::StatusCode::OK, Arc::clone(&self.body))
    }
}

impl Isolate for LargeStreamingService {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: tina_runtime::RuntimeCall<HttpRequest>,
        shard: SimShard,
    }

    fn handle(
        &mut self,
        _request: HttpRequest,
        _ctx: &mut Context<'_, SimShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.response())
    }

    fn handle_call(
        &mut self,
        _request: HttpRequest,
        call: tina::CallContext<'_, Self>,
    ) -> Effect<Self> {
        call.reply(self.response())
    }
}

impl LargeStreamingService {
    fn response(&self) -> HttpResponse {
        HttpResponse::stream_known_length(http::StatusCode::OK, BODY_LEN, self.source)
    }
}

#[derive(Clone, Copy)]
enum ResponseMode {
    Buffered,
    Streaming,
}

struct RunResult {
    trace_hash: u64,
    trace_len: usize,
    peer_output: Vec<u8>,
    flow_control_facts: usize,
    closed_stream_facts: usize,
}

fn run_pass(
    seed: u64,
    continue_with_credit: bool,
    concurrent: bool,
    mode: ResponseMode,
) -> RunResult {
    let bind_addr: SocketAddr = "127.0.0.1:19090".parse().unwrap();
    let peer_addr: SocketAddr = "10.0.0.1:59090".parse().unwrap();
    let inbound_chunks = scripted_peer_input(continue_with_credit, concurrent);
    let first_read_len = inbound_chunks[0].len();
    let config = SimulatorConfig {
        seed,
        tcp: ScriptedTcpConfig {
            pending_completion_capacity: 64,
            listeners: vec![ScriptedListenerConfig {
                bind_addr,
                local_addr: bind_addr,
                backlog_capacity: 4,
                peers: vec![ScriptedPeerConfig {
                    accept_after_step: 0,
                    peer_addr,
                    inbound_chunks,
                    inbound_capacity: 64 * 1024,
                    read_chunk_cap: Some(first_read_len),
                    // Force the server's write retry path while DATA is paced.
                    write_cap: 2 * 1024,
                    output_capacity: BODY_LEN * usize::from(concurrent) + BODY_LEN + 16 * 1024,
                }],
            }],
        },
        ..Default::default()
    };
    let body: Arc<[u8]> = expected_body().into();
    let mut sim = Simulator::new(SimShard, config);
    let service = match mode {
        ResponseMode::Buffered => {
            sim.register_with_mailbox_capacity(LargeBufferedService { body }, 8)
        }
        ResponseMode::Streaming => {
            let chunks = vec![body.as_ref().to_vec()].into_iter();
            let source =
                sim.register_with_mailbox_capacity(IterBodySource::<SimShard>::new(chunks), 8);
            sim.register_with_mailbox_capacity(LargeStreamingService { source }, 8)
        }
    };
    let server_config = Http2ServerConfig {
        limits: Http2Limits {
            initial_connection_window: 100_000,
            max_response_body_bytes: BODY_LEN,
            ..Http2Limits::default()
        },
        ..Http2ServerConfig::default()
    };
    let listener = sim.register_with_mailbox_capacity(
        Http2Listener::<SimShard>::new(bind_addr, service, server_config)
            .expect("valid HTTP/2 server config"),
        server_config.listener_mailbox_capacity,
    );
    sim.try_send(listener, Http2ListenerMsg::Start)
        .expect("start simulated HTTP/2 listener");
    drive_steps(&mut sim, 1024);
    sim.try_send(listener, Http2ListenerMsg::Stop)
        .expect("stop simulated HTTP/2 listener");
    drive_steps(&mut sim, 64);

    let trace_hash = stable_trace_hash(sim.trace().iter());
    let trace_len = sim.trace().len();
    let flow_control_facts = sim
        .trace()
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::FactObserved {
                    fact: RuntimeFact::Protocol(ProtocolFact::Http2FlowControlFull { .. })
                }
            )
        })
        .count();
    let closed_stream_facts = sim
        .trace()
        .iter()
        .filter(|event| {
            matches!(
                event.kind(),
                RuntimeEventKind::FactObserved {
                    fact: RuntimeFact::Protocol(ProtocolFact::Http2StreamClosed { .. })
                }
            )
        })
        .count();
    let artifact = sim.replay_artifact();
    let peer_output = artifact
        .observed_peer_output()
        .iter()
        .find(|output| output.peer_addr() == peer_addr)
        .map(ObservedPeerOutput::bytes)
        .expect("simulator captured HTTP/2 peer output")
        .to_vec();
    RunResult {
        trace_hash,
        trace_len,
        peer_output,
        flow_control_facts,
        closed_stream_facts,
    }
}

fn drive_steps(sim: &mut Simulator<SimShard>, budget: usize) {
    for _ in 0..budget {
        sim.step();
    }
}

fn scripted_peer_input(continue_with_credit: bool, concurrent: bool) -> Vec<Vec<u8>> {
    let mut handshake_and_request = Vec::new();
    handshake_and_request.extend_from_slice(CLIENT_PREFACE);
    push_frame(&mut handshake_and_request, FRAME_SETTINGS, 0, 0, &[]);
    push_frame(
        &mut handshake_and_request,
        FRAME_HEADERS,
        FLAG_END_HEADERS | FLAG_END_STREAM,
        1,
        &request_headers(),
    );
    if concurrent {
        push_frame(
            &mut handshake_and_request,
            FRAME_HEADERS,
            FLAG_END_HEADERS | FLAG_END_STREAM,
            3,
            &request_headers(),
        );
    }

    let remaining_credit = u32::try_from(BODY_LEN - 65_535).expect("test body delta fits u32");
    let mut credit = Vec::new();
    push_frame(
        &mut credit,
        FRAME_WINDOW_UPDATE,
        0,
        0,
        &remaining_credit.to_be_bytes(),
    );
    if concurrent {
        push_frame(
            &mut credit,
            FRAME_WINDOW_UPDATE,
            0,
            3,
            &remaining_credit.to_be_bytes(),
        );
    }
    push_frame(
        &mut credit,
        FRAME_WINDOW_UPDATE,
        0,
        1,
        &remaining_credit.to_be_bytes(),
    );
    // Scripted TCP reports EOF as soon as its inbound chunks drain. Keep one
    // deliberately incomplete, maximum-sized extension frame in flight so the
    // server has a realistic open peer while the service reply and short
    // writes run. The frame is never complete and is therefore never handled.
    let mut open_peer_stall = Vec::new();
    push_frame_header(&mut open_peer_stall, 16 * 1024, 0xff, 0, 0);
    open_peer_stall.resize(open_peer_stall.len() + 16 * 1024 - 1, 0);

    if continue_with_credit {
        vec![handshake_and_request, credit, open_peer_stall]
    } else {
        vec![handshake_and_request, open_peer_stall]
    }
}

fn request_headers() -> Vec<u8> {
    let mut block = Vec::new();
    for (name, value) in [
        (":method", "GET"),
        (":scheme", "http"),
        (":path", "/large"),
        (":authority", "localhost"),
    ] {
        block.push(0);
        block.push(u8::try_from(name.len()).expect("short test header name"));
        block.extend_from_slice(name.as_bytes());
        block.push(u8::try_from(value.len()).expect("short test header value"));
        block.extend_from_slice(value.as_bytes());
    }
    block
}

fn push_frame(out: &mut Vec<u8>, ty: u8, flags: u8, stream_id: u32, payload: &[u8]) {
    let len = payload.len();
    push_frame_header(out, len, ty, flags, stream_id);
    out.extend_from_slice(payload);
}

fn push_frame_header(out: &mut Vec<u8>, len: usize, ty: u8, flags: u8, stream_id: u32) {
    out.push(u8::try_from((len >> 16) & 0xff).expect("masked frame length"));
    out.push(u8::try_from((len >> 8) & 0xff).expect("masked frame length"));
    out.push(u8::try_from(len & 0xff).expect("masked frame length"));
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
}

fn expected_body() -> Vec<u8> {
    (0..BODY_LEN).map(|index| (index % 251) as u8).collect()
}

fn response_data(output: &[u8]) -> (Vec<u8>, usize) {
    let mut cursor = 0;
    let mut body = Vec::new();
    let mut terminal_data_frames = 0;
    while cursor + 9 <= output.len() {
        let len = (usize::from(output[cursor]) << 16)
            | (usize::from(output[cursor + 1]) << 8)
            | usize::from(output[cursor + 2]);
        let end = cursor + 9 + len;
        assert!(
            end <= output.len(),
            "simulated peer output ends mid-frame: cursor={cursor} payload_len={len} end={end} output_len={}",
            output.len()
        );
        let ty = output[cursor + 3];
        let flags = output[cursor + 4];
        let stream_id = u32::from_be_bytes(
            output[cursor + 5..cursor + 9]
                .try_into()
                .expect("four stream-id bytes"),
        ) & 0x7fff_ffff;
        if ty == FRAME_DATA && stream_id == 1 {
            assert!(len <= 16 * 1024, "DATA exceeds default peer frame cap");
            body.extend_from_slice(&output[cursor + 9..end]);
            if flags & FLAG_END_STREAM != 0 {
                terminal_data_frames += 1;
            }
        }
        cursor = end;
    }
    assert_eq!(cursor, output.len(), "trailing partial HTTP/2 frame");
    (body, terminal_data_frames)
}

fn initial_connection_credit(output: &[u8]) -> Option<u32> {
    let mut cursor = 0;
    while cursor + 9 <= output.len() {
        let len = (usize::from(output[cursor]) << 16)
            | (usize::from(output[cursor + 1]) << 8)
            | usize::from(output[cursor + 2]);
        let end = cursor + 9 + len;
        if end > output.len() {
            return None;
        }
        let ty = output[cursor + 3];
        let stream_id =
            u32::from_be_bytes(output[cursor + 5..cursor + 9].try_into().ok()?) & 0x7fff_ffff;
        if ty == FRAME_WINDOW_UPDATE && stream_id == 0 && len == 4 {
            return Some(
                u32::from_be_bytes(output[cursor + 9..end].try_into().ok()?) & 0x7fff_ffff,
            );
        }
        cursor = end;
    }
    None
}

#[test]
fn buffered_response_pressure_is_deterministic_and_bounded_to_initial_credit() {
    let first = run_pass(0xB0D1_5EED, false, false, ResponseMode::Buffered);
    let replay = run_pass(0xB0D1_5EED, false, false, ResponseMode::Buffered);

    assert_eq!(first.trace_hash, replay.trace_hash);
    assert_eq!(first.trace_len, replay.trace_len);
    assert_eq!(first.peer_output, replay.peer_output);
    assert_eq!(initial_connection_credit(&first.peer_output), Some(34_465));
    let (body, terminal_frames) = response_data(&first.peer_output);
    assert!(
        first.flow_control_facts > 0,
        "large buffered response must expose flow-control pressure in the DST trace; peer_output={} response_body={}",
        first.peer_output.len(),
        body.len()
    );

    assert_eq!(body, expected_body()[..65_535]);
    assert_eq!(
        terminal_frames, 0,
        "a response parked on flow control must not end the stream"
    );
    assert_eq!(
        first.closed_stream_facts, 1,
        "scripted peer EOF must close and clean up the parked stream exactly once"
    );
}

#[test]
fn streamed_response_zero_window_fact_is_deterministic_and_replays() {
    let first = run_pass(0x57EA_0EED, false, false, ResponseMode::Streaming);
    let replay = run_pass(0x57EA_0EED, false, false, ResponseMode::Streaming);

    assert_eq!(first.trace_hash, replay.trace_hash);
    assert_eq!(first.trace_len, replay.trace_len);
    assert_eq!(first.peer_output, replay.peer_output);
    assert!(
        first.flow_control_facts > 0,
        "streamed response must emit flow-control facts at zero connection credit"
    );
    let (body, terminal_frames) = response_data(&first.peer_output);
    assert_eq!(body, expected_body()[..65_535]);
    assert_eq!(terminal_frames, 0);
}

#[test]
fn credited_buffered_response_is_deterministic_under_short_writes() {
    let first = run_pass(0xC0ED_17ED, true, false, ResponseMode::Buffered);
    let replay = run_pass(0xC0ED_17ED, true, false, ResponseMode::Buffered);

    assert_eq!(first.trace_hash, replay.trace_hash);
    assert_eq!(first.trace_len, replay.trace_len);
    assert_eq!(first.peer_output, replay.peer_output);
    assert_eq!(
        first.closed_stream_facts, 1,
        "completed response must close its stream exactly once"
    );

    let (body, terminal_frames) = response_data(&first.peer_output);
    assert_eq!(body, expected_body());
    assert_eq!(terminal_frames, 1, "exactly one DATA frame ends the stream");
}

fn data_streams_after_initial_credit(output: &[u8]) -> Vec<u32> {
    let mut cursor = 0;
    let mut data_bytes = 0;
    let mut credited_streams = Vec::new();
    while cursor + 9 <= output.len() {
        let len = (usize::from(output[cursor]) << 16)
            | (usize::from(output[cursor + 1]) << 8)
            | usize::from(output[cursor + 2]);
        let end = cursor + 9 + len;
        assert!(end <= output.len(), "peer output ends mid-frame");
        let ty = output[cursor + 3];
        let stream_id =
            u32::from_be_bytes(output[cursor + 5..cursor + 9].try_into().unwrap()) & 0x7fff_ffff;
        if ty == FRAME_DATA && matches!(stream_id, 1 | 3) {
            if data_bytes >= 65_535 && credited_streams.len() < 2 {
                credited_streams.push(stream_id);
            }
            data_bytes += len;
        }
        cursor = end;
    }
    credited_streams
}

#[test]
fn concurrent_buffered_responses_share_credited_quanta_deterministically() {
    let first = run_pass(0xFA17_5EED, true, true, ResponseMode::Buffered);
    let replay = run_pass(0xFA17_5EED, true, true, ResponseMode::Buffered);

    assert_eq!(first.trace_hash, replay.trace_hash);
    assert_eq!(first.trace_len, replay.trace_len);
    assert_eq!(first.peer_output, replay.peer_output);
    let mut streams = data_streams_after_initial_credit(&first.peer_output);
    streams.sort_unstable();
    assert_eq!(
        streams,
        [1, 3],
        "one connection WINDOW_UPDATE must advance both ready responses"
    );
}
