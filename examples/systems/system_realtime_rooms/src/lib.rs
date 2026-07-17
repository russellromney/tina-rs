//! `system_realtime_rooms` — production-shaped WebSocket room with a recurring
//! liveness tick.
//!
//! Hosted on `LocalSystem` with typed WebSocket split-service delivery. Room
//! stats are actor-owned; the host reads them through a typed snapshot call.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::time::{Duration, Instant};

use http::Method;
use tina::prelude::*;
use tina_http::{
    AdmitOutcome, HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse,
    HttpServerConfig, SendOutcomeAction, WebSocketCloseCode, WebSocketError,
    WebSocketLimits, WebSocketMemberTable, WebSocketSessionControl, WebSocketSessionHandle,
    WebSocketSessionId, WebSocketSessionMsg, WebSocketSessionOutcome, websocket_upgrade,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, LocalSystemConfig, SplitServiceHandle,
    sleep,
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
    pub left_protocol: u64,
    pub left_timeout: u64,
    pub left_foreign: u64,
    pub left_shutdown: u64,
    pub presence_ticks: u64,
    pub presence_broadcasts_ok: u64,
    pub presence_broadcasts_full: u64,
    pub presence_broadcasts_stale: u64,
    pub presence_broadcasts_closed: u64,
    pub presence_broadcasts_protocol: u64,
    pub presence_broadcasts_timeout: u64,
    pub presence_broadcasts_foreign: u64,
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

/// The room accepted shutdown but did not settle every requested close before
/// the bounded host wait expired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomShutdownTimeout {
    pub stats: RoomStats,
}

impl std::fmt::Display for RoomShutdownTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "room shutdown did not settle within 2s: requested={} ok={} failed={}",
            self.stats.shutdown_close_requested,
            self.stats.shutdown_close_ok,
            self.stats.shutdown_close_failed
        )
    }
}

impl std::error::Error for RoomShutdownTimeout {}


/// Reserved tick generation used as a host snapshot request (never scheduled).
const SNAPSHOT_TICK: u64 = u64::MAX;
const SESSION_SEND_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Clone, Copy)]
struct RoomShard;

impl Shard for RoomShard {
    fn id(&self) -> ShardId {
        ShardId::new(0x11ff)
    }
}

struct Room {
    members: WebSocketMemberTable,
    last_seen: BTreeMap<WebSocketSessionId, Instant>,
    presence_tick: Duration,
    idle_evict: Duration,
    stats: RoomStats,
    tick_generation: u64,
    bootstrapped: bool,
    shutting_down: bool,
    _shard: PhantomData<RoomShard>,
}

#[tina_runtime::isolate(
    event = WebSocketSessionMsg,
    request = WebSocketSessionMsg,
    reply = WebSocketSessionOutcome,
    shard = RoomShard
)]
impl Room {
    fn handle_request(
        &mut self,
        msg: WebSocketSessionMsg,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        let now = call.now();
        match msg {
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(SNAPSHOT_TICK)) => {
                call.reply(WebSocketSessionOutcome::Text(serialize_stats(&self.stats)))
            }
            WebSocketSessionMsg::SessionOpen { session } => {
                call.reply(self.session_open_outcome(session, now))
            }
            WebSocketSessionMsg::SessionText { session_id, text } => {
                if !self.members.contains(session_id) {
                    return call.reply(WebSocketSessionOutcome::None);
                }
                self.last_seen.insert(session_id, now);
                self.stats.messages_in += 1;
                let body = format!("room:{text}");
                let effects = self.members.broadcast_text::<Self>(
                    Some(session_id),
                    body,
                    SESSION_SEND_TIMEOUT,
                );
                call.reply_and(WebSocketSessionOutcome::None, effects)
            }
            WebSocketSessionMsg::SessionBinary { session_id, .. } => {
                self.last_seen.insert(session_id, now);
                self.stats.messages_in += 1;
                call.reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::SessionClose {
                session_id,
                code,
                reason,
            } => {
                self.mark_gone(session_id, GoneReason::Peer);
                call.reply(WebSocketSessionOutcome::Close(code, reason))
            }
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Start) => {
                call.reply_and(WebSocketSessionOutcome::None, vec![self.on_bootstrap()])
            }
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(generation)) => {
                call.reply_and(
                    WebSocketSessionOutcome::None,
                    vec![self.on_tick(generation, now)],
                )
            }
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Drain) => call.reply_and(
                WebSocketSessionOutcome::None,
                vec![self.on_shutdown(
                    Some(WebSocketCloseCode(1001)),
                    b"server drain".to_vec(),
                )],
            ),
            WebSocketSessionMsg::Shutdown { code, reason } => {
                call.reply_and(WebSocketSessionOutcome::None, vec![self.on_shutdown(code, reason)])
            }
            _ => call.reply(WebSocketSessionOutcome::None),
        }
    }

    fn handle_event(
        &mut self,
        msg: WebSocketSessionMsg,
        ctx: &mut Context<'_, RoomShard, Self::Reply>,
    ) -> Effect<Self> {
        let now = ctx.now();
        match msg {
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Start) => self.on_bootstrap(),
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(generation))
                if generation != SNAPSHOT_TICK =>
            {
                self.on_tick(generation, now)
            }
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Drain) => {
                self.on_shutdown(Some(WebSocketCloseCode(1001)), b"server drain".to_vec())
            }
            WebSocketSessionMsg::SessionClosed { session_id, .. } => {
                self.mark_gone(session_id, GoneReason::Peer);
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::SessionPressure {
                session_id,
                error: WebSocketError::PeerClosed | WebSocketError::Closing,
            } => {
                self.mark_gone(session_id, GoneReason::Peer);
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::SessionPressure {
                session_id,
                error: WebSocketError::OutboundQueueFull | WebSocketError::OutboundBytesFull,
            } => {
                self.mark_gone(session_id, GoneReason::Slow);
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::SessionPressure { .. } => reply(WebSocketSessionOutcome::None),
            WebSocketSessionMsg::SendOutcome(outcome) => self.on_send_outcome(outcome),
            WebSocketSessionMsg::Shutdown { code, reason } => self.on_shutdown(code, reason),
            _ => reply(WebSocketSessionOutcome::None),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum GoneReason {
    Peer,
    Slow,
}

impl Room {
    fn on_bootstrap(&mut self) -> Effect<Self> {
        if self.bootstrapped {
            return reply(WebSocketSessionOutcome::None);
        }
        self.bootstrapped = true;
        self.stats.bootstrap_seen = true;
        self.schedule_tick()
    }

    fn schedule_tick(&mut self) -> Effect<Self> {
        if self.shutting_down {
            return reply(WebSocketSessionOutcome::None);
        }
        self.tick_generation = self.tick_generation.saturating_add(1);
        let tick_gen = self.tick_generation;
        sleep(self.presence_tick).then_service_event(move |_| {
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(tick_gen))
        })
    }

    fn on_tick(&mut self, generation: u64, now: Instant) -> Effect<Self> {
        if self.shutting_down || generation != self.tick_generation {
            return reply(WebSocketSessionOutcome::None);
        }
        self.stats.presence_ticks += 1;

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
                self.stats.left_idle += 1;
                self.stats.live_members = self.members.len();
                effects.push(handle.close_effect_service_event::<
                    Self,
                    WebSocketSessionMsg,
                    WebSocketSessionMsg,
                    _,
                >(
                    Some(WebSocketCloseCode(1001)),
                    b"idle".to_vec(),
                    SESSION_SEND_TIMEOUT,
                    WebSocketSessionMsg::SendOutcome,
                ));
            }
        }

        let payload = format!("tick:{}:{}", generation, self.members.len());
        effects.extend(self.members.broadcast_text::<Self>(
            None,
            payload,
            SESSION_SEND_TIMEOUT,
        ));
        effects.push(self.schedule_tick());
        batch(effects)
    }

    fn session_open_outcome(
        &mut self,
        session: WebSocketSessionHandle,
        now: Instant,
    ) -> WebSocketSessionOutcome {
        let session_id = session.session_id();
        if self.shutting_down {
            self.stats.rejected_shutdown += 1;
            return WebSocketSessionOutcome::Close(
                Some(WebSocketCloseCode(1001)),
                b"shutdown".to_vec(),
            );
        }
        match self.members.admit(session) {
            AdmitOutcome::Admitted => {
                self.last_seen.insert(session_id, now);
                self.stats.joined += 1;
                self.stats.live_members = self.members.len();
                if self.stats.live_members > self.stats.member_high_water {
                    self.stats.member_high_water = self.stats.live_members;
                }
                WebSocketSessionOutcome::Text(format!("join:{}", session_id.raw()))
            }
            AdmitOutcome::Full => {
                self.stats.rejected_full += 1;
                WebSocketSessionOutcome::Close(
                    Some(WebSocketCloseCode(1013)),
                    b"room full".to_vec(),
                )
            }
            AdmitOutcome::AlreadyMember => WebSocketSessionOutcome::Close(
                Some(WebSocketCloseCode(1011)),
                b"duplicate session".to_vec(),
            ),
        }
    }

    fn mark_gone(&mut self, session_id: WebSocketSessionId, reason: GoneReason) {
        let removed = self.members.remove(session_id).is_some();
        self.last_seen.remove(&session_id);
        if !removed {
            return;
        }
        self.stats.live_members = self.members.len();
        if self.shutting_down {
            self.stats.left_shutdown += 1;
        } else {
            match reason {
                GoneReason::Peer => self.stats.left_peer += 1,
                GoneReason::Slow => self.stats.left_slow += 1,
            }
        }
    }

    fn on_send_outcome(&mut self, outcome: tina_http::WebSocketSendOutcome) -> Effect<Self> {
        let session_id = outcome.session;
        let action = self.members.record_send_outcome(&outcome);
        self.record_send_action(Some(session_id), action);
        reply(WebSocketSessionOutcome::None)
    }

    fn record_send_action(
        &mut self,
        session_id: Option<WebSocketSessionId>,
        action: SendOutcomeAction,
    ) {
        let removed = !matches!(action, SendOutcomeAction::Ok | SendOutcomeAction::Stale);
        if removed {
            if let Some(session_id) = session_id {
                self.last_seen.remove(&session_id);
            }
            self.stats.live_members = self.members.len();
        }

        if self.shutting_down {
            match action {
                SendOutcomeAction::Ok => self.stats.shutdown_close_ok += 1,
                _ => self.stats.shutdown_close_failed += 1,
            }
            if removed {
                self.stats.left_shutdown += 1;
            }
        } else {
            match action {
                SendOutcomeAction::Ok => {
                self.stats.presence_broadcasts_ok += 1;
                self.stats.messages_out_ok += 1;
                }
                SendOutcomeAction::Stale => self.stats.presence_broadcasts_stale += 1,
                SendOutcomeAction::RemovedSlow => {
                    self.stats.presence_broadcasts_full += 1;
                    self.stats.left_slow += 1;
                }
                SendOutcomeAction::RemovedClosed => {
                    self.stats.presence_broadcasts_closed += 1;
                    self.stats.left_peer += 1;
                }
                SendOutcomeAction::RemovedProtocol => {
                    self.stats.presence_broadcasts_protocol += 1;
                    self.stats.left_protocol += 1;
                }
                SendOutcomeAction::RemovedTimeout => {
                    self.stats.presence_broadcasts_timeout += 1;
                    self.stats.left_timeout += 1;
                }
                SendOutcomeAction::RemovedForeign => {
                    self.stats.presence_broadcasts_foreign += 1;
                    self.stats.left_foreign += 1;
                }
            }
        }
    }

    fn on_shutdown(&mut self, code: Option<WebSocketCloseCode>, reason: Vec<u8>) -> Effect<Self> {
        self.shutting_down = true;
        self.stats.shutdown_started = true;
        let effects: Vec<Effect<Self>> =
            self.members
                .shutdown_close::<Self>(code, reason, SESSION_SEND_TIMEOUT);
        self.stats.shutdown_close_requested += effects.len() as u64;
        if effects.is_empty() {
            reply(WebSocketSessionOutcome::None)
        } else {
            batch(effects)
        }
    }
}

#[cfg(test)]
mod outcome_accounting_tests {
    use super::*;

    fn room() -> Room {
        Room {
            members: WebSocketMemberTable::new(1),
            last_seen: BTreeMap::new(),
            presence_tick: Duration::from_secs(1),
            idle_evict: Duration::from_secs(1),
            stats: RoomStats {
                member_capacity: 1,
                live_members: 1,
                ..RoomStats::default()
            },
            tick_generation: 0,
            bootstrapped: true,
            shutting_down: false,
            _shard: PhantomData,
        }
    }

    #[test]
    fn pressure_closed_timeout_foreign_and_stale_update_exact_counters() {
        let apply = |action| {
            let mut room = room();
            room.record_send_action(None, action);
            assert_eq!(room.stats.live_members, 0);
            room.stats
        };

        let full = apply(SendOutcomeAction::RemovedSlow);
        assert_eq!((full.presence_broadcasts_full, full.left_slow), (1, 1));
        let closed = apply(SendOutcomeAction::RemovedClosed);
        assert_eq!(
            (closed.presence_broadcasts_closed, closed.left_peer),
            (1, 1)
        );
        let protocol = apply(SendOutcomeAction::RemovedProtocol);
        assert_eq!(
            (
                protocol.presence_broadcasts_protocol,
                protocol.left_protocol
            ),
            (1, 1)
        );
        let timeout = apply(SendOutcomeAction::RemovedTimeout);
        assert_eq!(
            (timeout.presence_broadcasts_timeout, timeout.left_timeout),
            (1, 1)
        );
        let foreign = apply(SendOutcomeAction::RemovedForeign);
        assert_eq!(
            (foreign.presence_broadcasts_foreign, foreign.left_foreign),
            (1, 1)
        );

        let mut stale = room();
        stale.record_send_action(None, SendOutcomeAction::Stale);
        assert_eq!(stale.stats.live_members, 1);
        assert_eq!(stale.stats.presence_broadcasts_stale, 1);
    }

    #[test]
    fn shutdown_removal_is_counted_once_by_the_room() {
        let mut room = room();
        room.shutting_down = true;
        room.record_send_action(None, SendOutcomeAction::RemovedClosed);
        assert_eq!(room.stats.live_members, 0);
        assert_eq!(room.stats.left_shutdown, 1);
        assert_eq!(room.stats.shutdown_close_failed, 1);
        assert_eq!(room.stats.left_peer, 0);
    }
}

struct Gateway {
    room: SplitServiceHandle<WebSocketSessionMsg, WebSocketSessionMsg, WebSocketSessionOutcome>,
    limits: WebSocketLimits,
}

#[tina_runtime::isolate(
    request = HttpRequest,
    reply = HttpResponse,
    shard = RoomShard
)]
impl Gateway {
    fn handle_request(
        &mut self,
        request: HttpRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        call.reply(self.respond(request))
    }
}

impl Gateway {
    fn respond(&mut self, request: HttpRequest) -> HttpResponse {
        match (request.method.clone(), request.path.as_str()) {
            (Method::GET, "/ws") => match websocket_upgrade(&request, self.limits) {
                Ok(upgrade) => {
                    HttpResponse::websocket(upgrade.accept_split_service(self.room, self.limits))
                }
                Err(_) => HttpResponse::bad_request(),
            },
            (Method::GET, "/health") => {
                HttpResponse::with_body(http::StatusCode::OK, b"healthy".to_vec())
            }
            (Method::GET, "/report") => {
                let mut response = HttpResponse::with_body(
                    http::StatusCode::OK,
                    b"{}".to_vec(),
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
    format!(
        "{{\"member_capacity\":{},\"live_members\":{},\"member_high_water\":{},\"joined\":{},\"left_peer\":{},\"left_idle\":{},\"left_slow\":{},\"left_protocol\":{},\"left_timeout\":{},\"left_foreign\":{},\"left_shutdown\":{},\"presence_ticks\":{},\"presence_broadcasts_ok\":{},\"presence_broadcasts_full\":{},\"presence_broadcasts_stale\":{},\"presence_broadcasts_closed\":{},\"presence_broadcasts_protocol\":{},\"presence_broadcasts_timeout\":{},\"presence_broadcasts_foreign\":{},\"rejected_full\":{},\"rejected_shutdown\":{},\"messages_in\":{},\"messages_out_ok\":{},\"bootstrap_seen\":{},\"shutdown_started\":{},\"shutdown_close_requested\":{},\"shutdown_close_ok\":{},\"shutdown_close_failed\":{}}}",
        stats.member_capacity,
        stats.live_members,
        stats.member_high_water,
        stats.joined,
        stats.left_peer,
        stats.left_idle,
        stats.left_slow,
        stats.left_protocol,
        stats.left_timeout,
        stats.left_foreign,
        stats.left_shutdown,
        stats.presence_ticks,
        stats.presence_broadcasts_ok,
        stats.presence_broadcasts_full,
        stats.presence_broadcasts_stale,
        stats.presence_broadcasts_closed,
        stats.presence_broadcasts_protocol,
        stats.presence_broadcasts_timeout,
        stats.presence_broadcasts_foreign,
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

fn parse_stats_json(json: &str) -> Option<RoomStats> {
    fn num(json: &str, key: &str) -> Option<u64> {
        let pat = format!("\"{key}\":");
        let i = json.find(&pat)?;
        let rest = &json[i + pat.len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        rest[..end].parse().ok()
    }
    fn flag(json: &str, key: &str) -> bool {
        json.contains(&format!("\"{key}\":true"))
    }
    Some(RoomStats {
        member_capacity: num(json, "member_capacity")? as usize,
        live_members: num(json, "live_members")? as usize,
        member_high_water: num(json, "member_high_water")? as usize,
        joined: num(json, "joined")?,
        left_peer: num(json, "left_peer")?,
        left_idle: num(json, "left_idle")?,
        left_slow: num(json, "left_slow")?,
        left_protocol: num(json, "left_protocol")?,
        left_timeout: num(json, "left_timeout")?,
        left_foreign: num(json, "left_foreign")?,
        left_shutdown: num(json, "left_shutdown")?,
        presence_ticks: num(json, "presence_ticks")?,
        presence_broadcasts_ok: num(json, "presence_broadcasts_ok")?,
        presence_broadcasts_full: num(json, "presence_broadcasts_full")?,
        presence_broadcasts_stale: num(json, "presence_broadcasts_stale")?,
        presence_broadcasts_closed: num(json, "presence_broadcasts_closed")?,
        presence_broadcasts_protocol: num(json, "presence_broadcasts_protocol")?,
        presence_broadcasts_timeout: num(json, "presence_broadcasts_timeout")?,
        presence_broadcasts_foreign: num(json, "presence_broadcasts_foreign")?,
        rejected_full: num(json, "rejected_full")?,
        rejected_shutdown: num(json, "rejected_shutdown")?,
        messages_in: num(json, "messages_in")?,
        messages_out_ok: num(json, "messages_out_ok")?,
        bootstrap_seen: flag(json, "bootstrap_seen"),
        shutdown_started: flag(json, "shutdown_started"),
        shutdown_close_requested: num(json, "shutdown_close_requested")?,
        shutdown_close_ok: num(json, "shutdown_close_ok")?,
        shutdown_close_failed: num(json, "shutdown_close_failed")?,
    })
}

/// A running server, port-bound, with a reachable address.
pub struct RoomServer {
    addr: std::net::SocketAddr,
    app: LocalSystem<RoomShard, DefaultThreadedMailboxFactory>,
    listener: tina_http::HttpListenerAddress,
    room: SplitServiceHandle<WebSocketSessionMsg, WebSocketSessionMsg, WebSocketSessionOutcome>,
}

impl RoomServer {
    pub fn start(config: RunConfig) -> anyhow::Result<Self> {
        if config.room_mailbox_capacity == 0 {
            anyhow::bail!("room mailbox capacity must be greater than zero");
        }
        if config.gateway_mailbox_capacity == 0 {
            anyhow::bail!("gateway mailbox capacity must be greater than zero");
        }
        if config.listener_mailbox_capacity == 0 {
            anyhow::bail!("listener mailbox capacity must be greater than zero");
        }
        if config.connection_mailbox_capacity == 0 {
            anyhow::bail!("connection mailbox capacity must be greater than zero");
        }

        let app = LocalSystem::single_shard(RoomShard, DefaultThreadedMailboxFactory)
            .config(LocalSystemConfig {
                ingress_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..LocalSystemConfig::default()
            })
            .try_build()?;

        let limits = WebSocketLimits {
            max_queued_outbound_bytes: config.max_queued_outbound_bytes,
            outbound_frame_queue_capacity: config.outbound_frame_queue_capacity,
            ..Default::default()
        };

        let stats = RoomStats {
            member_capacity: config.member_capacity,
            ..Default::default()
        };

        let room = app
            .register_split_service_with_bootstrap::<
                Room,
                WebSocketSessionMsg,
                WebSocketSessionMsg,
                Infallible,
            >(
                Room {
                    members: WebSocketMemberTable::new(config.member_capacity),
                    last_seen: BTreeMap::new(),
                    presence_tick: Duration::from_millis(config.presence_tick_ms),
                    idle_evict: Duration::from_millis(config.idle_evict_after_ms),
                    stats,
                    tick_generation: 0,
                    bootstrapped: false,
                    shutting_down: false,
                    _shard: PhantomData,
                },
                config.room_mailbox_capacity,
                WebSocketSessionMsg::AppControl(WebSocketSessionControl::Start),
            )
            .map_err(|error| anyhow::anyhow!("register room: {error:?}"))?;

        let gateway = app
            .register_request_service::<Gateway, HttpRequest, Infallible>(
                Gateway { room, limits },
                config.gateway_mailbox_capacity,
            )
            .map_err(|error| anyhow::anyhow!("register gateway: {error:?}"))?;

        let mut server_config = HttpServerConfig::dev();
        server_config.limits = HttpLimits::default();
        server_config.service_call_timeout = Duration::from_secs(30);
        server_config.connection_mailbox_capacity = config.connection_mailbox_capacity;
        server_config.listener_mailbox_capacity = config.listener_mailbox_capacity;

        let listener = app
            .register_root::<_, Infallible>(
                HttpListener::<RoomShard, _>::for_request_service(
                    "127.0.0.1:0".parse().unwrap(),
                    gateway,
                    server_config,
                ),
                config.listener_mailbox_capacity,
            )
            .map_err(|error| anyhow::anyhow!("register listener: {error:?}"))?;

        let bound = app.observe_next_bound()?;
        app.try_send(listener, HttpListenerMsg::Start)
            .map_err(|error| anyhow::anyhow!("start listener: {error:?}"))?;
        let addr = bound
            .wait(Duration::from_secs(2))
            .map_err(|error| anyhow::anyhow!("wait for listener bind: {error:?}"))?;

        Ok(Self {
            addr,
            app,
            listener,
            room,
        })
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    pub fn snapshot(&self) -> RoomStats {
        match self.app.call_blocking_request(
            self.room.requests,
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(SNAPSHOT_TICK)),
            Duration::from_secs(2),
        ) {
            Ok(CallOutcome::Replied(WebSocketSessionOutcome::Text(json))) => {
                parse_stats_json(&json).unwrap_or_default()
            }
            _ => RoomStats::default(),
        }
    }

    pub fn wait_until(&self, timeout: Duration, mut f: impl FnMut(&RoomStats) -> bool) -> RoomStats {
        let deadline = Instant::now() + timeout;
        loop {
            let stats = self.snapshot();
            if f(&stats) || Instant::now() >= deadline {
                return stats;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn shutdown_room(&self) -> anyhow::Result<RoomStats> {
        self.app
            .try_send_event(
                self.room.events,
                WebSocketSessionMsg::Shutdown {
                    code: Some(WebSocketCloseCode(1001)),
                    reason: b"server shutdown".to_vec(),
                },
            )
            .map_err(|error| anyhow::anyhow!("send room shutdown: {error:?}"))?;
        let stats = self.wait_until(Duration::from_secs(2), |s| {
            s.shutdown_started
                && s.shutdown_close_requested > 0
                && s.shutdown_close_ok + s.shutdown_close_failed >= s.shutdown_close_requested
        });
        if !(stats.shutdown_started
            && stats.shutdown_close_requested > 0
            && stats.shutdown_close_ok + stats.shutdown_close_failed
                >= stats.shutdown_close_requested)
        {
            return Err(RoomShutdownTimeout { stats }.into());
        }
        Ok(stats)
    }

    pub fn stop(self) -> anyhow::Result<RoomStats> {
        let stats = self.snapshot();
        self.app
            .try_send(self.listener, HttpListenerMsg::Stop)
            .map_err(|error| anyhow::anyhow!("stop room listener: {error:?}"))?;
        let handle = self.app.shutdown_handle();
        let _ = handle.request_and_wait_report(Duration::from_secs(5));
        drop(self.app);
        Ok(stats)
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
    let server = RoomServer::start(config)?;
    let addr = server.addr();

    let a = test_client::connect(addr)?;
    let b = test_client::connect(addr)?;

    let reader = |mut client: test_client::Client| {
        std::thread::spawn(move || {
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

    let final_stats = server.stop()?;
    Ok(JoinAndTickReport {
        joined: final_stats.joined,
        broadcast_messages_seen: 0,
        tick_broadcasts_seen,
        bootstrap_seen: stats.bootstrap_seen,
        stats: final_stats,
    })
}

pub fn run_overflow(config: RunConfig) -> anyhow::Result<OverflowReport> {
    let mut config = config;
    config.member_capacity = 2;

    let server = RoomServer::start(config)?;
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
                rejected_full += 1;
                drop(client);
            }
        }
    }

    let _stats = server.wait_until(Duration::from_secs(1), |s| {
        s.joined as usize == admitted && (s.rejected_full as usize) >= rejected_full
    });

    drop(keep);
    let final_stats = server.stop()?;
    Ok(OverflowReport {
        admitted,
        rejected_full,
        stats: final_stats,
    })
}

pub fn run_shutdown(config: RunConfig) -> anyhow::Result<ShutdownReport> {
    let server = RoomServer::start(config)?;
    let addr = server.addr();
    let mut a = test_client::connect(addr)?;
    let mut b = test_client::connect(addr)?;
    let _ = a.recv_text_timeout(Duration::from_secs(1));
    let _ = b.recv_text_timeout(Duration::from_secs(1));

    let _ = server.wait_until(Duration::from_secs(1), |s| s.joined >= 2);

    let a_drainer = std::thread::spawn(move || (a.recv_close_timeout(Duration::from_secs(2)), a));
    let b_drainer = std::thread::spawn(move || (b.recv_close_timeout(Duration::from_secs(2)), b));

    let _stats_after_shutdown = server.shutdown_room()?;

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
    let final_stats = server.stop()?;
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
