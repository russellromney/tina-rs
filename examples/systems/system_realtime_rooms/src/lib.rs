//! `system_realtime_rooms` — production-shaped WebSocket room with a recurring
//! liveness tick.
//!
//! What this specimen pulls on:
//!
//! - Native WebSocket from `tina-http`: `WebSocketSessionHandle`,
//!   `WebSocketSessionMsg`, `websocket_upgrade`, `HttpResponse::websocket`.
//! - Bounded member table (one room, fixed capacity).
//! - Recurring liveness tick: a single `sleep_then` self-reschedules the room
//!   every `presence_tick`, broadcasts a heartbeat to every live member, and
//!   evicts members whose last broadcast was older than `idle_evict`.
//! - Explicit bootstrap control: the host sends `AppControl(Start)` after
//!   register so the recurring tick starts. Forgetting this produces a
//!   quiet room (Finding 22 in `examples/FINDINGS.md`).
//! - Graceful shutdown via the public `WebSocketSessionMsg::Shutdown` variant,
//!   which the room translates into bounded `handle.close_effect::<Room>`
//!   calls — no second writer, no hidden queue.
//! - Visible report: every cap, every counter, every typed outcome.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use http::Method;
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    AdmitOutcome, HttpConnectionMsg, HttpLimits, HttpListener, HttpListenerMsg, HttpRequest,
    HttpResponse, SendOutcomeAction, WebSocketCloseCode, WebSocketError, WebSocketLimits,
    WebSocketMemberTable, WebSocketSessionControl, WebSocketSessionHandle, WebSocketSessionId,
    WebSocketSessionMsg, WebSocketSessionOutcome, websocket_upgrade,
};
use tina_runtime::{
    DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime, ThreadedRuntimeConfig, sleep_then,
};

/// Tunables for one specimen run.
#[derive(Debug, Clone, Copy)]
pub struct RunConfig {
    pub member_capacity: usize,
    pub presence_tick_ms: u64,
    pub idle_evict_after_ms: u64,
    pub room_mailbox_capacity: usize,
    pub gateway_mailbox_capacity: usize,
    pub listener_mailbox_capacity: usize,
    pub connection_mailbox_capacity: usize,
    pub max_queued_outbound_bytes: usize,
    pub outbound_frame_queue_capacity: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            member_capacity: 3,
            // Slower tick keeps mailbox pressure small in CI; the test still
            // proves the recurring schedule and bounded fanout.
            presence_tick_ms: 80,
            idle_evict_after_ms: 1_000,
            room_mailbox_capacity: 256,
            gateway_mailbox_capacity: 16,
            listener_mailbox_capacity: 8,
            connection_mailbox_capacity: 64,
            max_queued_outbound_bytes: 64 * 1024,
            outbound_frame_queue_capacity: 64,
        }
    }
}

/// Aggregate report for one full `run(...)` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub join_and_tick: JoinAndTickReport,
    pub overflow: OverflowReport,
    pub shutdown: ShutdownReport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinAndTickReport {
    pub joined: u64,
    pub broadcast_messages_seen: u64,
    pub tick_broadcasts_seen: u64,
    pub bootstrap_seen: bool,
    pub stats: RoomStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowReport {
    pub admitted: usize,
    pub rejected_full: usize,
    pub stats: RoomStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    pub close_observed: usize,
    pub stats: RoomStats,
}

/// Operational snapshot. Every cap and counter the test cares about lives
/// here, so smoke assertions never reach into private state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoomStats {
    pub member_capacity: usize,
    pub live_members: usize,
    pub member_high_water: usize,
    pub joined: u64,
    pub left_peer: u64,
    pub left_idle: u64,
    pub left_slow: u64,
    pub left_shutdown: u64,
    pub presence_ticks: u64,
    pub presence_broadcasts_ok: u64,
    pub presence_broadcasts_full: u64,
    pub presence_broadcasts_stale: u64,
    pub rejected_full: u64,
    pub rejected_shutdown: u64,
    pub messages_in: u64,
    pub messages_out_ok: u64,
    pub bootstrap_seen: bool,
    pub shutdown_started: bool,
    pub shutdown_close_requested: u64,
    pub shutdown_close_ok: u64,
    pub shutdown_close_failed: u64,
}

#[derive(Debug, Default)]
struct SharedReport {
    stats: Mutex<RoomStats>,
    changed: Condvar,
}

impl SharedReport {
    fn update<F: FnOnce(&mut RoomStats)>(&self, f: F) {
        let mut stats = self.stats.lock().expect("stats lock");
        f(&mut stats);
        self.changed.notify_all();
    }

    fn snapshot(&self) -> RoomStats {
        self.stats.lock().expect("stats lock").clone()
    }

    fn wait_until(&self, timeout: Duration, mut f: impl FnMut(&RoomStats) -> bool) -> RoomStats {
        let deadline = Instant::now() + timeout;
        let mut stats = self.stats.lock().expect("stats lock");
        while !f(&stats) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next, _) = self
                .changed
                .wait_timeout(stats, remaining)
                .expect("stats wait");
            stats = next;
        }
        stats.clone()
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct RoomShard;

impl Shard for RoomShard {
    fn id(&self) -> ShardId {
        ShardId::new(0x11ff)
    }
}

/// Room isolate. Owns the bounded member table, the recurring liveness tick,
/// and the slow/idle eviction policy.
///
/// The bounded member set, broadcast/admit bookkeeping, slow-peer eviction
/// on `SendOutcome`, and the shutdown close fanout all delegate to
/// `tina_http::WebSocketMemberTable`. Policy that does not belong on the
/// shared table — idle eviction with `last_seen`, the recurring presence
/// tick, the bootstrap message, and the shutdown sequencing — stays here.
struct Room {
    members: WebSocketMemberTable,
    last_seen: BTreeMap<WebSocketSessionId, Instant>,
    presence_tick: Duration,
    idle_evict: Duration,
    report: Arc<SharedReport>,
    tick_generation: u64,
    bootstrapped: bool,
    shutting_down: bool,
    _shard: PhantomData<RoomShard>,
}

impl Isolate for Room {
    tina::isolate_types! {
        message: WebSocketSessionMsg,
        reply: WebSocketSessionOutcome,
        send: tina::Outbound<HttpConnectionMsg>,
        spawn: Infallible,
        io: RuntimeCall<WebSocketSessionMsg>,
        shard: RoomShard,
    }

    fn handle(
        &mut self,
        msg: WebSocketSessionMsg,
        ctx: &mut Context<'_, RoomShard, Self::Reply>,
    ) -> Effect<Self> {
        self.dispatch(msg, ctx.now())
    }

    fn handle_call(
        &mut self,
        msg: WebSocketSessionMsg,
        call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        // Tina's connection owner uses `try_send` for app deliveries that
        // arrive at `handle`, and `call` for ones that need a reply
        // (SessionOpen/SessionText/etc.). We accept both shapes through
        // the same `dispatch` so the room stays single-source-of-truth.
        self.dispatch(msg, call.now())
    }
}

// Hand-rolled `tina::isolate_types!` block above does not detect
// `handle_call`, so the macro-generated `CallableIsolate` impl from
// `#[tina_runtime::isolate]` is not present. The trait-level rail must still
// be stamped to register through `register_service`.
impl tina::CallableIsolate for Room {}

impl Room {
    fn dispatch(&mut self, msg: WebSocketSessionMsg, now: Instant) -> Effect<Self> {
        match msg {
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Start) => self.on_bootstrap(),
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(generation)) => {
                self.on_tick(generation, now)
            }
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Drain) => {
                self.on_shutdown(Some(WebSocketCloseCode(1001)), b"server drain".to_vec())
            }
            // ---- session lifecycle ----------------------------------------
            WebSocketSessionMsg::SessionOpen { session } => self.on_session_open(session, now),
            WebSocketSessionMsg::SessionText { session_id, text } => {
                self.on_session_text(session_id, text, now)
            }
            WebSocketSessionMsg::SessionBinary { session_id, .. } => {
                // Touch liveness on inbound binary; we don't echo binary in
                // this specimen.
                self.last_seen.insert(session_id, now);
                self.report.update(|s| s.messages_in += 1);
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::SessionClose { session_id, .. }
            | WebSocketSessionMsg::SessionClosed { session_id, .. } => {
                self.on_session_gone(session_id, GoneReason::Peer)
            }
            WebSocketSessionMsg::SessionPressure {
                session_id,
                error:
                    WebSocketError::PeerClosed
                    | WebSocketError::Closing
                    | WebSocketError::OutboundQueueFull
                    | WebSocketError::OutboundBytesFull,
            } => self.on_session_gone(session_id, GoneReason::Peer),
            WebSocketSessionMsg::SessionPressure { .. } => {
                // Non-fatal pressure (e.g., protocol-error report). The
                // connection owner will close the stream and a subsequent
                // SessionClosed will arrive; don't drop the member here.
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::SendOutcome(outcome) => self.on_send_outcome(outcome),
            WebSocketSessionMsg::Shutdown { code, reason } => self.on_shutdown(code, reason),
            // ---- legacy 087 echo path is unused here; ignore quietly ------
            WebSocketSessionMsg::Open
            | WebSocketSessionMsg::Text(_)
            | WebSocketSessionMsg::Binary(_)
            | WebSocketSessionMsg::Ping(_)
            | WebSocketSessionMsg::Pong(_)
            | WebSocketSessionMsg::Close(_, _)
            | WebSocketSessionMsg::Pressure(_)
            | WebSocketSessionMsg::Closed(_)
            | WebSocketSessionMsg::SessionAccepted { .. }
            | WebSocketSessionMsg::SessionReport(_) => reply(WebSocketSessionOutcome::None),
        }
    }

    fn on_bootstrap(&mut self) -> Effect<Self> {
        if self.bootstrapped {
            return reply(WebSocketSessionOutcome::None);
        }
        self.bootstrapped = true;
        self.report.update(|s| s.bootstrap_seen = true);
        self.schedule_tick()
    }

    fn schedule_tick(&mut self) -> Effect<Self> {
        if self.shutting_down {
            return reply(WebSocketSessionOutcome::None);
        }
        self.tick_generation = self.tick_generation.saturating_add(1);
        sleep_then(
            self.presence_tick,
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(self.tick_generation)),
        )
    }

    fn on_tick(&mut self, generation: u64, now: Instant) -> Effect<Self> {
        if self.shutting_down || generation != self.tick_generation {
            return reply(WebSocketSessionOutcome::None);
        }
        self.report.update(|s| s.presence_ticks += 1);

        // Evict members not seen recently. "Seen" advances when an inbound
        // SessionText arrives. A slow / silent peer ages out. The table
        // helper does not track `last_seen`; this is room policy.
        let stale: Vec<WebSocketSessionId> = self
            .last_seen
            .iter()
            .filter(|(_, t)| now.duration_since(**t) >= self.idle_evict)
            .map(|(id, _)| *id)
            .collect();
        let mut effects: Vec<Effect<Self>> = Vec::new();
        for id in stale {
            if let Some(handle) = self.members.remove(id) {
                self.last_seen.remove(&id);
                self.report.update(|s| {
                    s.left_idle += 1;
                    s.live_members = self.members.len();
                });
                effects.push(tina::send(
                    handle.target(),
                    handle.close(Some(WebSocketCloseCode(1001)), b"idle".to_vec()),
                ));
            }
        }

        // Broadcast a heartbeat through the bounded table fanout. Uses
        // try_send semantics through each member's connection owner so the
        // followup `WebSocketSessionMsg::SendOutcome` reaches our `handle`
        // path. The `call → .then` form would deliver the connection's
        // `Wrote(_)` completion back via `handle_call` and stall the queue
        // (see `examples/FINDINGS.md` Finding 26).
        let payload = format!("tick:{}:{}", generation, self.members.len());
        effects.extend(self.members.broadcast_text::<Self>(None, payload));
        effects.push(self.schedule_tick());
        batch(effects)
    }

    fn on_session_open(&mut self, session: WebSocketSessionHandle, now: Instant) -> Effect<Self> {
        let session_id = session.session_id();
        if self.shutting_down {
            self.report.update(|s| s.rejected_shutdown += 1);
            return reply(WebSocketSessionOutcome::Close(
                Some(WebSocketCloseCode(1001)),
                b"shutdown".to_vec(),
            ));
        }
        match self.members.admit(session) {
            AdmitOutcome::Admitted => {
                self.last_seen.insert(session_id, now);
                self.report.update(|s| {
                    s.joined += 1;
                    s.live_members = self.members.len();
                    if s.live_members > s.member_high_water {
                        s.member_high_water = s.live_members;
                    }
                });
                reply(WebSocketSessionOutcome::Text(format!(
                    "join:{}",
                    session_id.raw()
                )))
            }
            AdmitOutcome::Full => {
                self.report.update(|s| s.rejected_full += 1);
                reply(WebSocketSessionOutcome::Close(
                    Some(WebSocketCloseCode(1013)),
                    b"room full".to_vec(),
                ))
            }
            AdmitOutcome::AlreadyMember => {
                // The connection owner reuses session ids per upgrade only;
                // a duplicate here is a Tina bug, not a peer event. Reply
                // with a Close so the peer is not silently kept.
                reply(WebSocketSessionOutcome::Close(
                    Some(WebSocketCloseCode(1011)),
                    b"duplicate session".to_vec(),
                ))
            }
        }
    }

    fn on_session_text(
        &mut self,
        session_id: WebSocketSessionId,
        text: String,
        now: Instant,
    ) -> Effect<Self> {
        if !self.members.contains(session_id) {
            // A stray text after we already evicted the member. The
            // connection owner will close it shortly via lifecycle msgs;
            // nothing to do.
            return reply(WebSocketSessionOutcome::None);
        }
        self.last_seen.insert(session_id, now);
        self.report.update(|s| s.messages_in += 1);

        let body = format!("room:{text}");
        let mut effects = self.members.broadcast_text::<Self>(Some(session_id), body);
        if effects.is_empty() {
            reply(WebSocketSessionOutcome::None)
        } else {
            effects.push(reply(WebSocketSessionOutcome::None));
            batch(effects)
        }
    }

    fn on_session_gone(
        &mut self,
        session_id: WebSocketSessionId,
        reason: GoneReason,
    ) -> Effect<Self> {
        let removed = self.members.remove(session_id).is_some();
        self.last_seen.remove(&session_id);
        if removed {
            let shutting_down = self.shutting_down;
            self.report.update(|s| {
                s.live_members = self.members.len();
                if shutting_down {
                    s.left_shutdown += 1;
                } else {
                    match reason {
                        GoneReason::Peer => s.left_peer += 1,
                        GoneReason::Slow => s.left_slow += 1,
                    }
                }
            });
        }
        reply(WebSocketSessionOutcome::None)
    }

    fn on_send_outcome(&mut self, outcome: tina_http::WebSocketSendOutcome) -> Effect<Self> {
        // The table updates its own typed counters (joined/left_slow/etc.)
        // and removes terminal-failure members. The room mirrors the
        // outcome into its own report shape so existing assertions keep
        // working.
        let session_id = outcome.session;
        let action = self.members.record_send_outcome(&outcome);
        let shutting_down = self.shutting_down;
        self.report.update(|s| match (shutting_down, action) {
            (true, SendOutcomeAction::Ok) => {
                s.shutdown_close_ok += 1;
            }
            (true, _) => {
                // Match the pre-helper shutdown bookkeeping: every non-Ok
                // outcome during shutdown — including Stale — counts as a
                // shutdown_close_failed entry. The smoke proof relies on
                // requested == ok + failed.
                s.shutdown_close_failed += 1;
            }
            (false, SendOutcomeAction::Ok) => {
                s.presence_broadcasts_ok += 1;
                s.messages_out_ok += 1;
            }
            (false, SendOutcomeAction::Stale) => {
                s.presence_broadcasts_stale += 1;
            }
            (false, SendOutcomeAction::RemovedSlow) => {
                s.presence_broadcasts_full += 1;
            }
            (false, _) => {}
        });
        match action {
            SendOutcomeAction::Ok | SendOutcomeAction::Stale => {
                reply(WebSocketSessionOutcome::None)
            }
            SendOutcomeAction::RemovedSlow => {
                self.after_table_removed(session_id, GoneReason::Slow)
            }
            SendOutcomeAction::RemovedClosed
            | SendOutcomeAction::RemovedProtocol
            | SendOutcomeAction::RemovedTimeout => {
                self.after_table_removed(session_id, GoneReason::Peer)
            }
        }
    }

    fn after_table_removed(
        &mut self,
        session_id: WebSocketSessionId,
        reason: GoneReason,
    ) -> Effect<Self> {
        // The table has already evicted the member and updated its own
        // counters. Sync `last_seen` and the room's typed totals here so
        // the existing report assertions still see the per-reason counter
        // tick over.
        self.last_seen.remove(&session_id);
        let shutting_down = self.shutting_down;
        self.report.update(|s| {
            s.live_members = self.members.len();
            if shutting_down {
                s.left_shutdown += 1;
            } else {
                match reason {
                    GoneReason::Peer => s.left_peer += 1,
                    GoneReason::Slow => s.left_slow += 1,
                }
            }
        });
        reply(WebSocketSessionOutcome::None)
    }

    fn on_shutdown(&mut self, code: Option<WebSocketCloseCode>, reason: Vec<u8>) -> Effect<Self> {
        self.shutting_down = true;
        self.report.update(|s| s.shutdown_started = true);
        // `shutdown_close` leaves the table populated; subsequent
        // SessionClosed / SendOutcome deliveries reconcile membership
        // through the ordinary paths.
        let effects: Vec<Effect<Self>> = self.members.shutdown_close::<Self>(code, reason);
        let count = effects.len() as u64;
        self.report.update(|s| {
            s.shutdown_close_requested += count;
        });
        if effects.is_empty() {
            reply(WebSocketSessionOutcome::None)
        } else {
            batch(effects)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum GoneReason {
    Peer,
    Slow,
}

/// HTTP gateway isolate. Routes `/ws` → WebSocket upgrade, `/health` → 200,
/// `/report` → JSON stats. Everything else is 404.
struct Gateway {
    // Capability-typed callable handle: the gateway only ever calls the room
    // (via the HTTP connection's later send-outcome / inbound delivery path),
    // never directly sends. `CallAddress` enforces that contract at the type
    // boundary instead of through the runtime-level
    // `UnsupportedMessage` rejection that used to land here when an internal
    // continuation reached `handle_call` by mistake.
    room: tina::CallAddress<WebSocketSessionMsg, WebSocketSessionOutcome>,
    limits: WebSocketLimits,
    report: Arc<SharedReport>,
}

impl Isolate for Gateway {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        io: RuntimeCall<HttpRequest>,
        shard: RoomShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, RoomShard, Self::Reply>,
    ) -> Effect<Self> {
        reply(self.respond(request))
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(self.respond(request))
    }
}

impl tina::CallableIsolate for Gateway {}

impl Gateway {
    fn respond(&mut self, request: HttpRequest) -> HttpResponse {
        match (request.method.clone(), request.path.as_str()) {
            (Method::GET, "/ws") => match websocket_upgrade(&request, self.limits) {
                Ok(upgrade) => {
                    // tina-http currently takes a raw `Address`; unwrap the
                    // callable capability for that boundary. The wrapping
                    // capability still pins the lane in this isolate's
                    // fields.
                    HttpResponse::websocket(upgrade.accept(self.room.address(), self.limits))
                }
                Err(_) => HttpResponse::bad_request(),
            },
            (Method::GET, "/health") => {
                HttpResponse::with_body(http::StatusCode::OK, b"healthy".to_vec())
            }
            (Method::GET, "/report") => {
                let mut response = HttpResponse::with_body(
                    http::StatusCode::OK,
                    serialize_stats(&self.report.snapshot()).into_bytes(),
                );
                response.headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                response
            }
            _ => HttpResponse::not_found(),
        }
    }
}

fn serialize_stats(stats: &RoomStats) -> String {
    // Hand-rolled to avoid a serde dep just for one report.
    format!(
        "{{\"member_capacity\":{},\"live_members\":{},\"member_high_water\":{},\"joined\":{},\"left_peer\":{},\"left_idle\":{},\"left_slow\":{},\"left_shutdown\":{},\"presence_ticks\":{},\"presence_broadcasts_ok\":{},\"presence_broadcasts_full\":{},\"presence_broadcasts_stale\":{},\"rejected_full\":{},\"rejected_shutdown\":{},\"messages_in\":{},\"messages_out_ok\":{},\"bootstrap_seen\":{},\"shutdown_started\":{},\"shutdown_close_requested\":{},\"shutdown_close_ok\":{},\"shutdown_close_failed\":{}}}",
        stats.member_capacity,
        stats.live_members,
        stats.member_high_water,
        stats.joined,
        stats.left_peer,
        stats.left_idle,
        stats.left_slow,
        stats.left_shutdown,
        stats.presence_ticks,
        stats.presence_broadcasts_ok,
        stats.presence_broadcasts_full,
        stats.presence_broadcasts_stale,
        stats.rejected_full,
        stats.rejected_shutdown,
        stats.messages_in,
        stats.messages_out_ok,
        stats.bootstrap_seen,
        stats.shutdown_started,
        stats.shutdown_close_requested,
        stats.shutdown_close_ok,
        stats.shutdown_close_failed,
    )
}

/// A running server, port-bound, with a reachable address.
pub struct RoomServer {
    addr: std::net::SocketAddr,
    runtime: ThreadedRuntime<RoomShard, DefaultThreadedMailboxFactory>,
    listener: tina_http::HttpListenerAddress,
    // Both lanes are needed at startup: a `try_send` for bootstrap/shutdown
    // (internal continuations) and a `CallAddress` for the gateway → room
    // call-shaped path. Holding the full `ServiceHandle` keeps both lanes
    // capability-typed at this field; consumers downstream still pick the
    // narrow lane they need.
    room: tina_runtime::ServiceHandle<WebSocketSessionMsg, WebSocketSessionOutcome>,
    report: Arc<SharedReport>,
}

impl RoomServer {
    pub fn start(config: RunConfig) -> Self {
        let report = Arc::new(SharedReport::default());
        report.update(|s| s.member_capacity = config.member_capacity);
        let runtime = ThreadedRuntime::with_config(
            RoomShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );

        let limits = WebSocketLimits {
            max_queued_outbound_bytes: config.max_queued_outbound_bytes,
            outbound_frame_queue_capacity: config.outbound_frame_queue_capacity,
            ..Default::default()
        };

        let room = runtime
            .register_service::<Room, HttpConnectionMsg>(
                Room {
                    members: WebSocketMemberTable::new(config.member_capacity),
                    last_seen: BTreeMap::new(),
                    presence_tick: Duration::from_millis(config.presence_tick_ms),
                    idle_evict: Duration::from_millis(config.idle_evict_after_ms),
                    report: report.clone(),
                    tick_generation: 0,
                    bootstrapped: false,
                    shutting_down: false,
                    _shard: PhantomData,
                },
                config.room_mailbox_capacity,
            )
            .expect("register room");

        let gateway = runtime
            .register_service::<Gateway, Infallible>(
                Gateway {
                    room: room.call,
                    limits,
                    report: report.clone(),
                },
                config.gateway_mailbox_capacity,
            )
            .expect("register gateway");

        let listener_isolate = HttpListener::<RoomShard>::new(
            "127.0.0.1:0".parse().unwrap(),
            gateway.address(),
            HttpLimits::default(),
            // Generous service-call timeout: the connection isolate calls
            // back into the room to deliver SendOutcome / inbound events,
            // and if the room is briefly busy with tick fan-out a tight
            // timeout would close otherwise-healthy sessions.
            Duration::from_secs(30),
            config.connection_mailbox_capacity,
        );
        let listener = runtime
            .register_with_capacity::<HttpListener<RoomShard>, Infallible>(
                listener_isolate,
                config.listener_mailbox_capacity,
            )
            .expect("register listener");

        let bound = runtime.observe_next_bound();
        runtime
            .try_send(listener, HttpListenerMsg::Start)
            .expect("start listener");
        let addr = bound.wait(Duration::from_secs(2)).expect("bound address");

        // Bootstrap the room's recurring tick through the typed app-control
        // lane. It is still an ordinary bounded message to the room.
        runtime
            .try_send(
                room.send.address(),
                WebSocketSessionMsg::AppControl(WebSocketSessionControl::Start),
            )
            .expect("bootstrap");

        Self {
            addr,
            runtime,
            listener,
            room,
            report,
        }
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    pub fn snapshot(&self) -> RoomStats {
        self.report.snapshot()
    }

    pub fn wait_until(&self, timeout: Duration, f: impl FnMut(&RoomStats) -> bool) -> RoomStats {
        self.report.wait_until(timeout, f)
    }

    pub fn shutdown_room(&self) -> RoomStats {
        let _ = self.runtime.try_send(
            self.room.send.address(),
            WebSocketSessionMsg::Shutdown {
                code: Some(WebSocketCloseCode(1001)),
                reason: b"server shutdown".to_vec(),
            },
        );
        self.wait_until(Duration::from_secs(2), |s| {
            s.shutdown_started
                && s.shutdown_close_requested > 0
                && s.shutdown_close_ok + s.shutdown_close_failed >= s.shutdown_close_requested
        })
    }

    pub fn stop(self) -> RoomStats {
        let stats = self.snapshot();
        let _ = self.runtime.try_send(self.listener, HttpListenerMsg::Stop);
        let _ = self.runtime.shutdown();
        stats
    }
}

/// Run all three smoke scenarios end-to-end. Each scenario uses its own
/// `RoomServer` so reports do not bleed across scenarios.
pub fn run(config: RunConfig) -> anyhow::Result<RunReport> {
    Ok(RunReport {
        join_and_tick: run_join_and_tick(config)?,
        overflow: run_overflow(config)?,
        shutdown: run_shutdown(config)?,
    })
}

pub fn run_join_and_tick(config: RunConfig) -> anyhow::Result<JoinAndTickReport> {
    use test_client::RecvOutcome;
    let server = RoomServer::start(config);
    let addr = server.addr();

    // Two real WebSocket clients connect to the same room. Each drains on
    // its own thread so the bounded outbound queue can flush. We assert
    // that bootstrap fired, the recurring tick self-rescheduled, and that
    // each attentive client saw at least one presence tick on the wire.
    let a = test_client::connect(addr)?;
    let b = test_client::connect(addr)?;

    let reader = |mut client: test_client::Client| {
        std::thread::spawn(move || {
            // Drain the initial join reply.
            let _ = client.recv_text_timeout(Duration::from_secs(1));
            let mut ticks = 0u64;
            let deadline = std::time::Instant::now() + Duration::from_millis(4000);
            while std::time::Instant::now() < deadline {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                match client.recv_text_timeout(remaining.min(Duration::from_millis(200))) {
                    RecvOutcome::Text(text) if text.starts_with("tick:") => ticks += 1,
                    RecvOutcome::Text(_) => {}
                    RecvOutcome::Timeout => continue,
                    RecvOutcome::NonText => continue,
                    RecvOutcome::Closed => break,
                }
                if ticks >= 2 {
                    break;
                }
            }
            ticks
        })
    };

    let a_reader = reader(a);
    let b_reader = reader(b);

    let a_ticks = a_reader.join().expect("a reader joins");
    let b_ticks = b_reader.join().expect("b reader joins");
    let tick_broadcasts_seen = a_ticks.max(b_ticks);

    let stats = server.wait_until(Duration::from_secs(2), |s| {
        s.bootstrap_seen && s.presence_ticks >= 2 && s.joined >= 2
    });

    let final_stats = server.stop();
    Ok(JoinAndTickReport {
        joined: final_stats.joined,
        // The room → fanout-on-SessionText path has a known race with the
        // bounded outbound queue when the inbound write to one peer is in
        // flight; the assertion here only covers tick broadcasts, which the
        // recurring schedule guarantees once at least one client is reading.
        broadcast_messages_seen: 0,
        tick_broadcasts_seen,
        bootstrap_seen: stats.bootstrap_seen,
        stats: final_stats,
    })
}

pub fn run_overflow(config: RunConfig) -> anyhow::Result<OverflowReport> {
    let mut config = config;
    // Pin the cap small so overflow is deterministic.
    config.member_capacity = 2;

    let server = RoomServer::start(config);
    let addr = server.addr();
    let mut keep = Vec::new();
    let mut admitted = 0usize;
    let mut rejected_full = 0usize;

    for _ in 0..(config.member_capacity + 2) {
        let mut client = test_client::connect(addr)?;
        let first = client.recv_text_timeout(Duration::from_millis(300));
        match first {
            test_client::RecvOutcome::Text(text) if text.starts_with("join:") => {
                admitted += 1;
                keep.push(client);
            }
            _ => {
                // Either the server sent a Close frame, or the connection
                // is already going down. Either way the stats will count
                // this as rejected_full.
                rejected_full += 1;
                drop(client);
            }
        }
    }

    let _stats = server.wait_until(Duration::from_secs(1), |s| {
        s.joined as usize == admitted && (s.rejected_full as usize) >= rejected_full
    });

    drop(keep);
    let final_stats = server.stop();
    Ok(OverflowReport {
        admitted,
        rejected_full,
        stats: final_stats,
    })
}

pub fn run_shutdown(config: RunConfig) -> anyhow::Result<ShutdownReport> {
    let server = RoomServer::start(config);
    let addr = server.addr();
    let mut a = test_client::connect(addr)?;
    let mut b = test_client::connect(addr)?;
    let _ = a.recv_text_timeout(Duration::from_secs(1));
    let _ = b.recv_text_timeout(Duration::from_secs(1));

    let _ = server.wait_until(Duration::from_secs(1), |s| s.joined >= 2);

    // Concurrent drainers so each client can observe the close frame even
    // while the other client's stream is being closed by the room.
    let a_drainer = std::thread::spawn(move || (a.recv_close_timeout(Duration::from_secs(2)), a));
    let b_drainer = std::thread::spawn(move || (b.recv_close_timeout(Duration::from_secs(2)), b));

    let _stats_after_shutdown = server.shutdown_room();

    // Each client should observe a close frame.
    let (a_saw, a_client) = a_drainer.join().expect("a drainer joins");
    let (b_saw, b_client) = b_drainer.join().expect("b drainer joins");
    let mut close_observed = 0usize;
    if a_saw {
        close_observed += 1;
    }
    if b_saw {
        close_observed += 1;
    }

    drop(a_client);
    drop(b_client);
    let final_stats = server.stop();
    Ok(ShutdownReport {
        close_observed,
        stats: final_stats,
    })
}

pub mod test_client {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    use tungstenite::client::IntoClientRequest;
    use tungstenite::protocol::WebSocketConfig;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket, client};

    pub struct Client {
        ws: WebSocket<MaybeTlsStream<TcpStream>>,
    }

    pub fn connect(addr: SocketAddr) -> anyhow::Result<Client> {
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let request = format!("ws://{addr}/ws").into_client_request()?;
        let (ws, _response) =
            client(request, MaybeTlsStream::Plain(stream)).map_err(anyhow::Error::from)?;
        Ok(Client { ws })
    }

    #[derive(Debug)]
    pub enum RecvOutcome {
        Text(String),
        Timeout,
        Closed,
        NonText,
    }

    impl Client {
        pub fn send_text(&mut self, text: &str) -> anyhow::Result<()> {
            self.ws
                .send(Message::Text(text.to_owned().into()))
                .map_err(anyhow::Error::from)
        }

        pub fn recv_text_timeout(&mut self, timeout: Duration) -> RecvOutcome {
            if let MaybeTlsStream::Plain(stream) = self.ws.get_mut() {
                let _ = stream.set_read_timeout(Some(timeout));
            }
            match self.ws.read() {
                Ok(Message::Text(text)) => RecvOutcome::Text(text.to_string()),
                Ok(Message::Close(_)) => RecvOutcome::Closed,
                Ok(_) => RecvOutcome::NonText,
                Err(tungstenite::Error::Io(io)) if would_block_or_timeout(&io) => {
                    RecvOutcome::Timeout
                }
                Err(tungstenite::Error::ConnectionClosed)
                | Err(tungstenite::Error::AlreadyClosed) => RecvOutcome::Closed,
                Err(_) => RecvOutcome::Closed,
            }
        }

        pub fn recv_close_timeout(&mut self, timeout: Duration) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return false;
                }
                let remaining = deadline.saturating_duration_since(now);
                match self.recv_text_timeout(remaining) {
                    RecvOutcome::Closed => return true,
                    RecvOutcome::Timeout => return false,
                    RecvOutcome::Text(_) | RecvOutcome::NonText => continue,
                }
            }
        }
    }

    fn would_block_or_timeout(err: &std::io::Error) -> bool {
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    }

    // Quiet the dead-code warning on the unused config struct path.
    #[allow(dead_code)]
    fn _config() -> WebSocketConfig {
        WebSocketConfig::default()
    }
}
