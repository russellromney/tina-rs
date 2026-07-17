//! WebSocket room specimen: LocalSystem host, typed split-service delivery,
//! actor-owned terminal report (no SharedReport on the public host path).

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use http::Method;
use tina::prelude::*;
use tina::{reply_to, send_event};
use tina_http::{
    HttpConnectionMsg, HttpListener, HttpListenerMsg, HttpRequest, HttpResponse, HttpServerConfig,
    HttpsListener, HttpsListenerMsg, HttpsReady, HttpsServerConfig, HttpsStartupError,
    TlsServerIdentity, WebSocketCloseCode, WebSocketError, WebSocketLimits, WebSocketSendError,
    WebSocketSessionControl, WebSocketSessionHandle, WebSocketSessionId, WebSocketSessionMsg,
    WebSocketSessionOutcome, websocket_upgrade,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, LocalSystemConfig, SplitServiceHandle,
    call_request, sleep,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RoomReport {
    pub active_rooms: usize,
    pub room_capacity: usize,
    pub member_capacity: usize,
    pub joined: usize,
    pub left: usize,
    pub rejected_origin: usize,
    pub rejected_auth: usize,
    pub rejected_subprotocol: usize,
    pub rejected_full: usize,
    pub rejected_shutdown: usize,
    pub broadcast_ok: usize,
    pub broadcast_full: usize,
    pub broadcast_closed: usize,
    pub broadcast_timeout: usize,
    pub broadcast_foreign: usize,
    pub slow_peer_closed: usize,
    pub live_members: usize,
    pub shutdown_started: bool,
    pub shutdown_close_requested: usize,
    pub shutdown_close_ok: usize,
    pub shutdown_close_failed: usize,
    pub stale_handle_rejected: bool,
    pub refill_after_close: bool,
    pub selected_subprotocol_seen: bool,
    pub session_report_ok: usize,
    pub session_report_stale: usize,
    pub session_high_water: usize,
    pub room_high_water: usize,
    pub queued_frame_high_water: usize,
    pub queued_byte_high_water: usize,
    pub app_close_seen: bool,
    pub peer_close_seen: bool,
    pub protocol_close_seen: bool,
    pub timeout_close_seen: bool,
    pub client_a_received: bool,
    pub client_b_received: bool,
}

impl RoomReport {
    fn to_json(&self) -> String {
        format!(
            "{{\"active_rooms\":{},\"room_capacity\":{},\"member_capacity\":{},\"joined\":{},\"left\":{},\"rejected_origin\":{},\"rejected_auth\":{},\"rejected_subprotocol\":{},\"rejected_full\":{},\"rejected_shutdown\":{},\"broadcast_ok\":{},\"broadcast_full\":{},\"broadcast_closed\":{},\"broadcast_timeout\":{},\"broadcast_foreign\":{},\"slow_peer_closed\":{},\"live_members\":{},\"shutdown_started\":{},\"shutdown_close_requested\":{},\"shutdown_close_ok\":{},\"shutdown_close_failed\":{},\"stale_handle_rejected\":{},\"refill_after_close\":{},\"selected_subprotocol_seen\":{},\"session_report_ok\":{},\"session_report_stale\":{},\"session_high_water\":{},\"room_high_water\":{},\"queued_frame_high_water\":{},\"queued_byte_high_water\":{},\"app_close_seen\":{},\"peer_close_seen\":{},\"protocol_close_seen\":{},\"timeout_close_seen\":{},\"client_a_received\":{},\"client_b_received\":{}}}",
            self.active_rooms,
            self.room_capacity,
            self.member_capacity,
            self.joined,
            self.left,
            self.rejected_origin,
            self.rejected_auth,
            self.rejected_subprotocol,
            self.rejected_full,
            self.rejected_shutdown,
            self.broadcast_ok,
            self.broadcast_full,
            self.broadcast_closed,
            self.broadcast_timeout,
            self.broadcast_foreign,
            self.slow_peer_closed,
            self.live_members,
            self.shutdown_started,
            self.shutdown_close_requested,
            self.shutdown_close_ok,
            self.shutdown_close_failed,
            self.stale_handle_rejected,
            self.refill_after_close,
            self.selected_subprotocol_seen,
            self.session_report_ok,
            self.session_report_stale,
            self.session_high_water,
            self.room_high_water,
            self.queued_frame_high_water,
            self.queued_byte_high_water,
            self.app_close_seen,
            self.peer_close_seen,
            self.protocol_close_seen,
            self.timeout_close_seen,
            self.client_a_received,
            self.client_b_received
        )
    }
}

fn parse_report_json(json: &str) -> Option<RoomReport> {
    fn num(json: &str, key: &str) -> Option<usize> {
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
    Some(RoomReport {
        active_rooms: num(json, "active_rooms")?,
        room_capacity: num(json, "room_capacity")?,
        member_capacity: num(json, "member_capacity")?,
        joined: num(json, "joined")?,
        left: num(json, "left")?,
        rejected_origin: num(json, "rejected_origin")?,
        rejected_auth: num(json, "rejected_auth")?,
        rejected_subprotocol: num(json, "rejected_subprotocol")?,
        rejected_full: num(json, "rejected_full")?,
        rejected_shutdown: num(json, "rejected_shutdown")?,
        broadcast_ok: num(json, "broadcast_ok")?,
        broadcast_full: num(json, "broadcast_full")?,
        broadcast_closed: num(json, "broadcast_closed")?,
        broadcast_timeout: num(json, "broadcast_timeout")?,
        broadcast_foreign: num(json, "broadcast_foreign")?,
        slow_peer_closed: num(json, "slow_peer_closed")?,
        live_members: num(json, "live_members")?,
        shutdown_started: flag(json, "shutdown_started"),
        shutdown_close_requested: num(json, "shutdown_close_requested")?,
        shutdown_close_ok: num(json, "shutdown_close_ok")?,
        shutdown_close_failed: num(json, "shutdown_close_failed")?,
        stale_handle_rejected: flag(json, "stale_handle_rejected"),
        refill_after_close: flag(json, "refill_after_close"),
        selected_subprotocol_seen: flag(json, "selected_subprotocol_seen"),
        session_report_ok: num(json, "session_report_ok")?,
        session_report_stale: num(json, "session_report_stale")?,
        session_high_water: num(json, "session_high_water")?,
        room_high_water: num(json, "room_high_water")?,
        queued_frame_high_water: num(json, "queued_frame_high_water")?,
        queued_byte_high_water: num(json, "queued_byte_high_water")?,
        app_close_seen: flag(json, "app_close_seen"),
        peer_close_seen: flag(json, "peer_close_seen"),
        protocol_close_seen: flag(json, "protocol_close_seen"),
        timeout_close_seen: flag(json, "timeout_close_seen"),
        client_a_received: flag(json, "client_a_received"),
        client_b_received: flag(json, "client_b_received"),
    })
}

/// The room accepted shutdown but did not settle every requested close before
/// the bounded host wait expired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomShutdownTimeout {
    pub report: RoomReport,
}

impl std::fmt::Display for RoomShutdownTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "room shutdown did not settle within 2s: requested={} ok={} failed={}",
            self.report.shutdown_close_requested,
            self.report.shutdown_close_ok,
            self.report.shutdown_close_failed
        )
    }
}

impl std::error::Error for RoomShutdownTimeout {}

/// Reserved tick generation used as a host snapshot request (never scheduled).
const SNAPSHOT_TICK: u64 = u64::MAX;

const BROWSER_CLIENT: &str = include_str!("../browser_client.html");
const ROOM_CREATE_CONTROL: &str = "__room_create__";
const ROOM_IDLE_EXPIRE_CONTROL: &str = "__room_idle_expire__:";
const ROOM_FORCE_IDLE_EXPIRE: &str = "__room_force_idle_expire__";
const REPORT_REJECT_ORIGIN: &str = "__report_reject_origin__";
const REPORT_REJECT_AUTH: &str = "__report_reject_auth__";
const REPORT_REJECT_SUBPROTOCOL: &str = "__report_reject_subprotocol__";
const REPORT_REJECT_SHUTDOWN: &str = "__report_reject_shutdown__";
const REPORT_CLIENT_A: &str = "__report_client_a__";
const REPORT_CLIENT_B: &str = "__report_client_b__";
const GATEWAY_MARK_SHUTDOWN: &str = "/__host_mark_shutdown";

#[derive(Debug, Default, Clone, Copy)]
struct DemoShard;

impl Shard for DemoShard {
    fn id(&self) -> ShardId {
        ShardId::new(987)
    }
}

#[derive(Debug, Clone, Default)]
pub struct AdmissionPolicy {
    pub allowed_origin: Option<String>,
    pub required_bearer_token: Option<String>,
    pub require_subprotocol: bool,
}

impl AdmissionPolicy {
    fn origin_allowed(&self, request: &HttpRequest) -> bool {
        let Some(allowed) = self.allowed_origin.as_deref() else {
            return true;
        };
        request
            .headers
            .get(http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|origin| origin == allowed)
    }

    fn auth_allowed(&self, request: &HttpRequest) -> bool {
        let Some(token) = self.required_bearer_token.as_deref() else {
            return true;
        };
        let bearer_ok = request
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == format!("Bearer {token}"));
        let cookie_ok = request
            .headers
            .get(http::header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.split(';').any(|part| {
                    let part = part.trim();
                    part == format!("room_token={token}")
                })
            });
        bearer_ok || cookie_ok
    }
}

type RoomHandle =
    SplitServiceHandle<WebSocketSessionMsg, WebSocketSessionMsg, WebSocketSessionOutcome>;

enum GatewayMsg {
    Http(HttpRequest),
    RoomSnapshot {
        req: tina::RequestContext<HttpResponse>,
        outcome: CallOutcome<WebSocketSessionOutcome>,
    },
    IdleSnapshot {
        generation: u64,
        outcome: CallOutcome<WebSocketSessionOutcome>,
    },
}

impl From<HttpRequest> for GatewayMsg {
    fn from(request: HttpRequest) -> Self {
        Self::Http(request)
    }
}

struct Gateway {
    room: RoomHandle,
    limits: WebSocketLimits,
    admission: AdmissionPolicy,
    room_active: bool,
    shutdown_started: bool,
    idle_room_expiry: Duration,
    idle_generation: u64,
}

#[tina_runtime::isolate(
    message = GatewayMsg,
    reply = HttpResponse,
    send = tina::Outbound<tina::ServiceMessage<WebSocketSessionMsg, WebSocketSessionMsg>>,
    shard = DemoShard
)]
impl Gateway {
    fn handle(
        &mut self,
        msg: GatewayMsg,
        _ctx: &mut Context<'_, DemoShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            GatewayMsg::Http(request) => {
                if let Some(generation) = idle_generation_from_path(&request.path) {
                    return self.handle_idle_deadline(generation);
                }
                let is_delete = request.method == Method::DELETE && request.path == "/rooms/default";
                let is_create = request.method == Method::POST && request.path == "/rooms/default";
                let is_host_shutdown = request.method == Method::POST
                    && request.path == GATEWAY_MARK_SHUTDOWN;
                let (response, side) = self.response_for(request);
                let mut effects = vec![reply(response)];
                effects.extend(side);
                if is_delete {
                    effects.push(self.room_delete_effect());
                } else if is_create {
                    effects.push(self.room_create_effect());
                } else if is_host_shutdown {
                    effects.push(self.room_host_shutdown_effect());
                }
                batch(effects)
            }
            GatewayMsg::RoomSnapshot { req, outcome } => {
                let body = match outcome {
                    CallOutcome::Replied(WebSocketSessionOutcome::Text(json)) => json.into_bytes(),
                    _ => b"{}".to_vec(),
                };
                let mut response = HttpResponse::with_body(http::StatusCode::OK, body);
                response.headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                reply_to(req, response)
            }
            GatewayMsg::IdleSnapshot {
                generation,
                outcome,
            } => self.apply_idle_snapshot(generation, outcome),
        }
    }

    fn handle_call(&mut self, msg: GatewayMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            GatewayMsg::Http(request) => {
                if let Some(generation) = idle_generation_from_path(&request.path) {
                    return batch(vec![
                        self.handle_idle_deadline(generation),
                        call.reply(HttpResponse::with_body(http::StatusCode::OK, Vec::new())),
                    ]);
                }
                if request.method == Method::GET && request.path == "/room-report" {
                    return call
                        .defer(call_request(
                            self.room.requests,
                            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(
                                SNAPSHOT_TICK,
                            )),
                            Duration::from_secs(1),
                        ))
                        .reply(|req, outcome| GatewayMsg::RoomSnapshot { req, outcome });
                }
                let is_delete = request.method == Method::DELETE && request.path == "/rooms/default";
                let is_create = request.method == Method::POST && request.path == "/rooms/default";
                let is_host_shutdown = request.method == Method::POST
                    && request.path == GATEWAY_MARK_SHUTDOWN;
                let (response, side) = self.response_for(request);
                let mut effects = vec![call.reply(response), self.arm_idle_deadline()];
                effects.extend(side);
                if is_delete {
                    effects.push(self.room_delete_effect());
                } else if is_create {
                    effects.push(self.room_create_effect());
                } else if is_host_shutdown {
                    effects.push(self.room_host_shutdown_effect());
                }
                batch(effects)
            }
            GatewayMsg::RoomSnapshot { req, outcome } => {
                let body = match outcome {
                    CallOutcome::Replied(WebSocketSessionOutcome::Text(json)) => json.into_bytes(),
                    _ => b"{}".to_vec(),
                };
                let mut response = HttpResponse::with_body(http::StatusCode::OK, body);
                response.headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                batch(vec![
                    reply_to(req, response),
                    call.reply(HttpResponse::with_body(http::StatusCode::OK, Vec::new())),
                ])
            }
            GatewayMsg::IdleSnapshot {
                generation,
                outcome,
            } => batch(vec![
                self.apply_idle_snapshot(generation, outcome),
                call.reply(HttpResponse::with_body(http::StatusCode::OK, Vec::new())),
            ]),
        }
    }
}

impl Gateway {
    fn response_for(&mut self, request: HttpRequest) -> (HttpResponse, Vec<Effect<Self>>) {
        if request.method == Method::POST && request.path == GATEWAY_MARK_SHUTDOWN {
            self.shutdown_started = true;
            return (
                HttpResponse::with_body(http::StatusCode::OK, b"marked".to_vec()),
                Vec::new(),
            );
        }
        if request.method == Method::GET && request.path == "/room" {
            if !self.room_active || self.shutdown_started {
                return (
                    HttpResponse::service_unavailable(),
                    vec![self.note_reject(REPORT_REJECT_SHUTDOWN)],
                );
            }
            if !self.admission.origin_allowed(&request) {
                return (
                    HttpResponse::with_body(
                        http::StatusCode::FORBIDDEN,
                        b"bad origin".to_vec(),
                    ),
                    vec![self.note_reject(REPORT_REJECT_ORIGIN)],
                );
            }
            if !self.admission.auth_allowed(&request) {
                return (
                    HttpResponse::with_body(
                        http::StatusCode::UNAUTHORIZED,
                        b"bad auth".to_vec(),
                    ),
                    vec![self.note_reject(REPORT_REJECT_AUTH)],
                );
            }
            let response = match websocket_upgrade(&request, self.limits) {
                Ok(upgrade)
                    if upgrade
                        .offered_subprotocols()
                        .iter()
                        .any(|protocol| protocol == "tina.room.v1") =>
                {
                    match upgrade.accept_split_service_subprotocol(
                        self.room,
                        self.limits,
                        "tina.room.v1",
                    ) {
                        Ok(accept) => HttpResponse::websocket(accept),
                        Err(_) => HttpResponse::bad_request(),
                    }
                }
                Ok(_) if self.admission.require_subprotocol => {
                    return (
                        HttpResponse::bad_request(),
                        vec![self.note_reject(REPORT_REJECT_SUBPROTOCOL)],
                    );
                }
                Ok(upgrade) => {
                    HttpResponse::websocket(upgrade.accept_split_service(self.room, self.limits))
                }
                Err(_) => HttpResponse::bad_request(),
            };
            (response, Vec::new())
        } else if request.method == Method::POST && request.path == "/rooms/default" {
            self.room_active = true;
            self.shutdown_started = false;
            (
                HttpResponse::with_body(http::StatusCode::CREATED, b"created".to_vec()),
                Vec::new(),
            )
        } else if request.method == Method::DELETE && request.path == "/rooms/default" {
            self.delete_room();
            (
                HttpResponse::with_body(http::StatusCode::OK, b"deleted".to_vec()),
                Vec::new(),
            )
        } else if request.method == Method::GET && request.path == "/" {
            let mut response =
                HttpResponse::with_body(http::StatusCode::OK, BROWSER_CLIENT.as_bytes().to_vec());
            response.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/html; charset=utf-8"),
            );
            (response, Vec::new())
        } else if request.method == Method::GET && request.path == "/room-report" {
            // Handled via nested room snapshot in handle_call; message-form
            // path returns empty JSON if it lands here.
            let mut response =
                HttpResponse::with_body(http::StatusCode::OK, b"{}".to_vec());
            response.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            (response, Vec::new())
        } else if request.method == Method::GET && request.path == "/health" {
            (
                HttpResponse::with_body(http::StatusCode::OK, b"healthy".to_vec()),
                Vec::new(),
            )
        } else if request.method == Method::GET && request.path == "/ready" {
            if self.shutdown_started {
                (HttpResponse::service_unavailable(), Vec::new())
            } else {
                (
                    HttpResponse::with_body(http::StatusCode::OK, b"accepting".to_vec()),
                    Vec::new(),
                )
            }
        } else {
            (HttpResponse::not_found(), Vec::new())
        }
    }

    fn note_reject(&self, control: &'static str) -> Effect<Self> {
        send_event(
            self.room.events,
            WebSocketSessionMsg::Text(control.to_string()),
        )
    }

    fn delete_room(&mut self) {
        if !self.room_active {
            return;
        }
        self.room_active = false;
        self.shutdown_started = true;
    }

    fn room_delete_effect(&self) -> Effect<Self> {
        send_event(
            self.room.events,
            WebSocketSessionMsg::Shutdown {
                code: Some(WebSocketCloseCode(1001)),
                reason: b"room delete".to_vec(),
            },
        )
    }

    fn room_host_shutdown_effect(&self) -> Effect<Self> {
        send_event(
            self.room.events,
            WebSocketSessionMsg::Shutdown {
                code: Some(WebSocketCloseCode(1001)),
                reason: b"server shutdown".to_vec(),
            },
        )
    }

    fn room_create_effect(&self) -> Effect<Self> {
        send_event(
            self.room.events,
            WebSocketSessionMsg::Text(ROOM_CREATE_CONTROL.to_string()),
        )
    }

    fn arm_idle_deadline(&mut self) -> Effect<Self> {
        if !self.room_active || self.idle_room_expiry.is_zero() {
            return noop();
        }
        self.idle_generation = self.idle_generation.saturating_add(1);
        let generation = self.idle_generation;
        sleep(self.idle_room_expiry).then(move |_| {
            GatewayMsg::Http(idle_deadline_request(generation))
        })
    }

    fn handle_idle_deadline(&mut self, generation: u64) -> Effect<Self> {
        if generation != self.idle_generation || !self.room_active {
            return noop();
        }
        // Snapshot room membership before expiring so a live member is not
        // cut off by a gateway timer that armed while the room still had traffic.
        call_request(
            self.room.requests,
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(SNAPSHOT_TICK)),
            Duration::from_secs(1),
        )
        .then(move |outcome| GatewayMsg::IdleSnapshot {
            generation,
            outcome,
        })
    }

    fn apply_idle_snapshot(
        &mut self,
        generation: u64,
        outcome: CallOutcome<WebSocketSessionOutcome>,
    ) -> Effect<Self> {
        if generation != self.idle_generation || !self.room_active {
            return noop();
        }
        let live_members = match outcome {
            CallOutcome::Replied(WebSocketSessionOutcome::Text(json)) => {
                parse_report_json(&json).map(|r| r.live_members).unwrap_or(1)
            }
            _ => 1,
        };
        if live_members == 0 {
            self.delete_room();
            // Align room-owned active_rooms / shutdown flags with gateway.
            return send_event(
                self.room.events,
                WebSocketSessionMsg::Text(ROOM_FORCE_IDLE_EXPIRE.to_string()),
            );
        }
        noop()
    }
}

fn idle_deadline_request(generation: u64) -> HttpRequest {
    HttpRequest {
        method: Method::GET,
        path: format!("/__idle_expire/{generation}"),
        version: http::Version::HTTP_11,
        headers: http::HeaderMap::new(),
        body: tina_http::HttpRequestBody::Buffered(Vec::new()),
    }
}

fn idle_generation_from_path(path: &str) -> Option<u64> {
    path.strip_prefix("/__idle_expire/")?.parse().ok()
}

struct Room {
    members: BTreeMap<WebSocketSessionId, WebSocketSessionHandle>,
    member_capacity: usize,
    idle_room_expiry: Duration,
    idle_generation: u64,
    report: RoomReport,
    first_closed: Option<WebSocketSessionId>,
    stale_probe: Option<WebSocketSessionHandle>,
    pending_stale_probe: Option<WebSocketSessionId>,
    shutting_down: bool,
    deleting: bool,
}

#[tina_runtime::isolate(
    event = WebSocketSessionMsg,
    request = WebSocketSessionMsg,
    reply = WebSocketSessionOutcome,
    send = tina::Outbound<HttpConnectionMsg>,
    shard = DemoShard
)]
impl Room {
    fn handle_request(
        &mut self,
        msg: WebSocketSessionMsg,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match msg {
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(tick))
                if tick == SNAPSHOT_TICK =>
            {
                call.reply(WebSocketSessionOutcome::Text(self.report.to_json()))
            }
            WebSocketSessionMsg::SessionOpen { session } => {
                let (outcome, effects) = self.session_open_result(session);
                call.reply_and(outcome, effects)
            }
            WebSocketSessionMsg::SessionText { session_id, text } => {
                let effects = self.session_text_effects(session_id, text);
                call.reply_and(WebSocketSessionOutcome::None, effects)
            }
            WebSocketSessionMsg::SessionBinary { bytes, .. } => {
                call.reply(WebSocketSessionOutcome::Binary(bytes))
            }
            WebSocketSessionMsg::SessionClose {
                session_id,
                code,
                reason,
            } => {
                let effect = self.handle_session_close(session_id);
                call.reply_and(WebSocketSessionOutcome::Close(code, reason), vec![effect])
            }
            WebSocketSessionMsg::Shutdown { code, reason } => {
                call.reply_and(
                    WebSocketSessionOutcome::None,
                    vec![self.handle_shutdown(code, reason)],
                )
            }
            other => {
                // Request-lane fallback for control texts that arrive as calls.
                call.reply_and(
                    WebSocketSessionOutcome::None,
                    vec![self.handle_room_event(other)],
                )
            }
        }
    }

    fn handle_event(
        &mut self,
        msg: WebSocketSessionMsg,
        _ctx: &mut Context<'_, DemoShard, Self::Reply>,
    ) -> Effect<Self> {
        self.handle_room_event(msg)
    }
}

impl Room {
    fn session_open_result(
        &mut self,
        session: WebSocketSessionHandle,
    ) -> (WebSocketSessionOutcome, Vec<Effect<Self>>) {
        self.idle_generation = self.idle_generation.saturating_add(1);
        let session_id = session.session_id();
        if self.shutting_down {
            self.report.rejected_shutdown += 1;
            return (
                WebSocketSessionOutcome::Close(
                    Some(WebSocketCloseCode(1001)),
                    b"room shutting down".to_vec(),
                ),
                Vec::new(),
            );
        }
        if self.members.len() < self.member_capacity {
            let mut effects = Vec::new();
            if self.first_closed.is_some() {
                self.report.refill_after_close = true;
                if self.pending_stale_probe.is_none()
                    && let Some(stale) = self.stale_probe.take()
                {
                    let stale_session = stale.session_id();
                    self.pending_stale_probe = Some(stale_session);
                    effects.push(stale.text_effect_service_event::<
                        Self,
                        WebSocketSessionMsg,
                        WebSocketSessionMsg,
                        _,
                    >(
                        "stale-proof",
                        Duration::from_secs(1),
                        WebSocketSessionMsg::SendOutcome,
                    ));
                }
            }
            self.members.insert(session_id, session);
            self.report.joined += 1;
            self.report.live_members = self.members.len();
            self.report.session_high_water =
                self.report.session_high_water.max(self.members.len());
            (
                WebSocketSessionOutcome::Text(format!("join:{}", session_id.raw())),
                effects,
            )
        } else {
            self.report.rejected_full += 1;
            (
                WebSocketSessionOutcome::Close(
                    Some(WebSocketCloseCode(1013)),
                    b"room full".to_vec(),
                ),
                Vec::new(),
            )
        }
    }

    fn handle_session_open(&mut self, session: WebSocketSessionHandle) -> Effect<Self> {
        let (outcome, effects) = self.session_open_result(session);
        if effects.is_empty() {
            reply(outcome)
        } else {
            let mut all = effects;
            all.push(reply(outcome));
            batch(all)
        }
    }

    fn session_text_effects(
        &mut self,
        session_id: WebSocketSessionId,
        text: String,
    ) -> Vec<Effect<Self>> {
        if text == "__report__"
            && let Some(handle) = self.members.get(&session_id).copied()
        {
            return vec![handle.report_effect_service_event::<
                Self,
                WebSocketSessionMsg,
                WebSocketSessionMsg,
                _,
            >(
                Duration::from_secs(1),
                WebSocketSessionMsg::SessionReport,
            )];
        }
        let mut effects = Vec::new();
        for (target_id, handle) in self.members.iter() {
            if *target_id != session_id {
                effects.push(handle.text_effect_service_event::<
                    Self,
                    WebSocketSessionMsg,
                    WebSocketSessionMsg,
                    _,
                >(
                    format!("room:{text}"),
                    Duration::from_secs(1),
                    WebSocketSessionMsg::SendOutcome,
                ));
            }
        }
        effects
    }

    fn handle_session_text(&mut self, session_id: WebSocketSessionId, text: String) -> Effect<Self> {
        let effects = self.session_text_effects(session_id, text);
        if effects.is_empty() {
            reply(WebSocketSessionOutcome::None)
        } else {
            let mut all = effects;
            all.push(reply(WebSocketSessionOutcome::None));
            batch(all)
        }
    }

    fn handle_session_close(&mut self, session_id: WebSocketSessionId) -> Effect<Self> {
        let mut removed_member = false;
        if let Some(handle) = self.members.remove(&session_id)
            && self.stale_probe.is_none()
        {
            self.stale_probe = Some(handle);
            removed_member = true;
        }
        self.first_closed = Some(session_id);
        self.report.left += 1;
        self.report.live_members = self.members.len();
        self.report.peer_close_seen = true;
        if self.deleting && self.members.is_empty() {
            self.shutting_down = false;
            self.deleting = false;
        }
        self.after_possible_leave(removed_member)
    }

    fn handle_shutdown(
        &mut self,
        code: Option<WebSocketCloseCode>,
        reason: Vec<u8>,
    ) -> Effect<Self> {
        let room_delete = reason.as_slice() == b"room delete";
        self.shutting_down = true;
        self.deleting = room_delete;
        let effects = self
            .members
            .values()
            .copied()
            .map(|handle| {
                handle.close_effect_service_event::<Self, WebSocketSessionMsg, WebSocketSessionMsg, _>(
                    code,
                    reason.clone(),
                    Duration::from_secs(1),
                    WebSocketSessionMsg::SendOutcome,
                )
            })
            .collect::<Vec<_>>();
        self.report.shutdown_started = true;
        self.report.shutdown_close_requested += effects.len();
        if effects.is_empty() {
            if self.deleting {
                self.shutting_down = false;
                self.deleting = false;
            }
            // DELETE / idle expiry should drop active room accounting.
            if room_delete || reason.as_slice() == b"server shutdown" {
                // host shutdown keeps active_rooms for report continuity
                if room_delete {
                    self.report.active_rooms = 0;
                }
            }
            reply(WebSocketSessionOutcome::None)
        } else {
            if room_delete {
                self.report.active_rooms = 0;
            }
            batch(effects)
        }
    }

    fn after_possible_leave(&mut self, removed_member: bool) -> Effect<Self> {
        if removed_member && self.members.is_empty() {
            self.arm_idle_expiry()
        } else {
            reply(WebSocketSessionOutcome::None)
        }
    }

    fn arm_idle_expiry(&mut self) -> Effect<Self> {
        if self.idle_room_expiry.is_zero() {
            return reply(WebSocketSessionOutcome::None);
        }
        self.idle_generation = self.idle_generation.saturating_add(1);
        let generation = self.idle_generation;
        sleep(self.idle_room_expiry).then_service_event(move |_| {
            WebSocketSessionMsg::Text(format!("{ROOM_IDLE_EXPIRE_CONTROL}{generation}"))
        })
    }

    fn expire_idle_room(&mut self, generation: &str) -> bool {
        let Ok(generation) = generation.parse::<u64>() else {
            return false;
        };
        if generation != self.idle_generation || !self.members.is_empty() {
            return true;
        }
        self.shutting_down = true;
        self.deleting = false;
        self.report.active_rooms = 0;
        self.report.shutdown_started = true;
        true
    }

    fn handle_room_event(&mut self, msg: WebSocketSessionMsg) -> Effect<Self> {
        match msg {
            WebSocketSessionMsg::SessionOpen { session } => self.handle_session_open(session),
            WebSocketSessionMsg::SessionText { session_id, text } => {
                self.handle_session_text(session_id, text)
            }
            WebSocketSessionMsg::SendOutcome(outcome) => {
                let stale_probe = self.pending_stale_probe == Some(outcome.session);
                if stale_probe {
                    self.pending_stale_probe = None;
                }
                match outcome.result {
                    Ok(()) => {
                        if self.shutting_down {
                            self.report.shutdown_close_ok += 1;
                            self.report.app_close_seen = true;
                        } else {
                            self.report.broadcast_ok += 1;
                        }
                    }
                    Err(
                        WebSocketSendError::OutboundQueueFull
                        | WebSocketSendError::OutboundBytesFull,
                    ) => {
                        if self.shutting_down {
                            self.report.shutdown_close_failed += 1;
                        } else if stale_probe {
                            self.report.stale_handle_rejected = true;
                        } else {
                            self.report.broadcast_full += 1;
                            self.report.slow_peer_closed += 1;
                        }
                    }
                    Err(WebSocketSendError::Closed | WebSocketSendError::Stale) => {
                        if self.shutting_down {
                            self.report.shutdown_close_failed += 1;
                        } else if stale_probe
                            || matches!(outcome.result, Err(WebSocketSendError::Stale))
                        {
                            self.report.stale_handle_rejected = true;
                        } else {
                            self.report.broadcast_closed += 1;
                        }
                    }
                    Err(WebSocketSendError::Closing) => {
                        if self.shutting_down {
                            self.report.shutdown_close_failed += 1;
                        } else if stale_probe {
                            self.report.stale_handle_rejected = true;
                        } else {
                            self.report.broadcast_closed += 1;
                        }
                    }
                    Err(WebSocketSendError::Protocol) => {
                        if self.shutting_down {
                            self.report.shutdown_close_failed += 1;
                        } else if stale_probe {
                            self.report.stale_handle_rejected = true;
                        } else {
                            self.report.broadcast_closed += 1;
                        }
                    }
                    Err(WebSocketSendError::Timeout) => {
                        if self.shutting_down {
                            self.report.shutdown_close_failed += 1;
                        } else if stale_probe {
                            self.report.stale_handle_rejected = true;
                        } else {
                            self.report.broadcast_timeout += 1;
                        }
                    }
                    Err(WebSocketSendError::ForeignSystem { .. }) => {
                        if self.shutting_down {
                            self.report.shutdown_close_failed += 1;
                        } else if stale_probe {
                            self.report.stale_handle_rejected = true;
                        } else {
                            self.report.broadcast_foreign += 1;
                        }
                    }
                }
                let mut removed_member = false;
                if outcome.result.is_err() && !stale_probe {
                    if let Some(handle) = self.members.remove(&outcome.session)
                        && self.stale_probe.is_none()
                    {
                        self.stale_probe = Some(handle);
                    }
                    self.first_closed = Some(outcome.session);
                    removed_member = true;
                    self.report.left += 1;
                    self.report.live_members = self.members.len();
                }
                self.after_possible_leave(removed_member)
            }
            WebSocketSessionMsg::SessionReport(outcome) => {
                match outcome.result {
                    Ok(report) => {
                        self.report.session_report_ok += 1;
                        self.report.queued_frame_high_water = self
                            .report
                            .queued_frame_high_water
                            .max(report.queued_outbound_frames);
                        self.report.queued_byte_high_water = self
                            .report
                            .queued_byte_high_water
                            .max(report.queued_outbound_bytes);
                    }
                    Err(WebSocketSendError::Stale) => {
                        self.report.session_report_stale += 1;
                        self.report.stale_handle_rejected = true;
                    }
                    Err(_) => {}
                }
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Shutdown { code, reason } => self.handle_shutdown(code, reason),
            WebSocketSessionMsg::SessionClose { session_id, .. }
            | WebSocketSessionMsg::SessionClosed { session_id, .. } => {
                self.handle_session_close(session_id)
            }
            WebSocketSessionMsg::SessionPressure {
                session_id,
                error: error @ (WebSocketError::PeerClosed | WebSocketError::Closing),
            } => {
                let mut removed_member = false;
                if let Some(handle) = self.members.remove(&session_id)
                    && self.stale_probe.is_none()
                {
                    self.stale_probe = Some(handle);
                    removed_member = true;
                }
                self.first_closed = Some(session_id);
                self.report.left += 1;
                self.report.live_members = self.members.len();
                match error {
                    WebSocketError::Timeout => self.report.timeout_close_seen = true,
                    WebSocketError::ProtocolError
                    | WebSocketError::InvalidClosePayload
                    | WebSocketError::ClientFrameUnmasked
                    | WebSocketError::InvalidOpcode(_) => {
                        self.report.protocol_close_seen = true
                    }
                    _ => {}
                }
                if self.deleting && self.members.is_empty() {
                    self.shutting_down = false;
                    self.deleting = false;
                }
                self.after_possible_leave(removed_member)
            }
            WebSocketSessionMsg::SessionPressure { error, .. } => {
                match error {
                    WebSocketError::Timeout => self.report.timeout_close_seen = true,
                    WebSocketError::ProtocolError
                    | WebSocketError::InvalidClosePayload
                    | WebSocketError::ClientFrameUnmasked
                    | WebSocketError::InvalidOpcode(_) => {
                        self.report.protocol_close_seen = true
                    }
                    _ => {}
                }
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text) if text == ROOM_CREATE_CONTROL => {
                self.shutting_down = false;
                self.deleting = false;
                self.idle_generation = self.idle_generation.saturating_add(1);
                self.report.active_rooms = 1;
                self.report.shutdown_started = false;
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text)
                if text.strip_prefix(ROOM_IDLE_EXPIRE_CONTROL).is_some() =>
            {
                if let Some(generation) = text.strip_prefix(ROOM_IDLE_EXPIRE_CONTROL) {
                    self.expire_idle_room(generation);
                }
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text) if text == ROOM_FORCE_IDLE_EXPIRE => {
                if self.members.is_empty() {
                    self.shutting_down = true;
                    self.deleting = false;
                    self.report.active_rooms = 0;
                    self.report.shutdown_started = true;
                }
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text) if text == REPORT_REJECT_ORIGIN => {
                self.report.rejected_origin += 1;
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text) if text == REPORT_REJECT_AUTH => {
                self.report.rejected_auth += 1;
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text) if text == REPORT_REJECT_SUBPROTOCOL => {
                self.report.rejected_subprotocol += 1;
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text) if text == REPORT_REJECT_SHUTDOWN => {
                self.report.rejected_shutdown += 1;
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text) if text == REPORT_CLIENT_A => {
                self.report.client_a_received = true;
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text) if text == REPORT_CLIENT_B => {
                self.report.client_b_received = true;
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::SessionAccepted {
                selected_subprotocol,
                ..
            } => {
                if selected_subprotocol.as_deref() == Some("tina.room.v1") {
                    self.report.selected_subprotocol_seen = true;
                }
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::SessionBinary { bytes, .. } => {
                reply(WebSocketSessionOutcome::Binary(bytes))
            }
            WebSocketSessionMsg::Open
            | WebSocketSessionMsg::Text(_)
            | WebSocketSessionMsg::Binary(_)
            | WebSocketSessionMsg::Ping(_)
            | WebSocketSessionMsg::Pong(_)
            | WebSocketSessionMsg::Close(_, _)
            | WebSocketSessionMsg::Pressure(_)
            | WebSocketSessionMsg::Closed(_)
            | WebSocketSessionMsg::AppControl(_) => reply(WebSocketSessionOutcome::None),
        }
    }
}

pub struct RoomServer {
    addr: SocketAddr,
    app: LocalSystem<DemoShard, DefaultThreadedMailboxFactory>,
    listener: tina_http::HttpListenerAddress,
    gateway: tina::Address<GatewayMsg, HttpResponse>,
    room: RoomHandle,
}

#[derive(Debug, Clone)]
pub struct RoomServerConfig {
    pub limits: WebSocketLimits,
    pub room_capacity: usize,
    pub member_capacity: usize,
    pub idle_room_expiry: Duration,
    pub admission: AdmissionPolicy,
    pub room_mailbox_capacity: usize,
    pub gateway_mailbox_capacity: usize,
    pub listener_mailbox_capacity: usize,
    pub connection_mailbox_capacity: usize,
}

impl Default for RoomServerConfig {
    fn default() -> Self {
        Self {
            limits: WebSocketLimits {
                max_queued_outbound_bytes: 1024,
                broadcast_fanout_max_targets: 2,
                close_handshake_timeout: Duration::from_millis(50),
                ..Default::default()
            },
            room_capacity: 1,
            member_capacity: 3,
            idle_room_expiry: Duration::from_secs(60),
            admission: AdmissionPolicy::default(),
            room_mailbox_capacity: 16,
            gateway_mailbox_capacity: 16,
            listener_mailbox_capacity: 8,
            connection_mailbox_capacity: 16,
        }
    }
}

impl RoomServer {
    pub fn start() -> anyhow::Result<Self> {
        Self::start_with(RoomServerConfig::default())
    }

    pub fn start_with(config: RoomServerConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.room_capacity == 1,
            "specimen_websocket_room supports one bounded named room"
        );
        let app = LocalSystem::single_shard(DemoShard, DefaultThreadedMailboxFactory)
            .config(LocalSystemConfig {
                ingress_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..LocalSystemConfig::default()
            })
            .try_build()?;

        let report = RoomReport {
            active_rooms: 1,
            room_capacity: config.room_capacity,
            member_capacity: config.member_capacity,
            room_high_water: 1,
            ..RoomReport::default()
        };

        let room = app
            .register_split_service::<
                Room,
                WebSocketSessionMsg,
                WebSocketSessionMsg,
                HttpConnectionMsg,
            >(
                Room {
                    members: BTreeMap::new(),
                    member_capacity: config.member_capacity,
                    idle_room_expiry: config.idle_room_expiry,
                    idle_generation: 0,
                    report,
                    first_closed: None,
                    stale_probe: None,
                    pending_stale_probe: None,
                    shutting_down: false,
                    deleting: false,
                },
                config.room_mailbox_capacity,
            )
            .map_err(|error| anyhow::anyhow!("register room: {error:?}"))?;

        let gateway = app
            .register_root::<Gateway, tina::ServiceMessage<WebSocketSessionMsg, WebSocketSessionMsg>>(
                Gateway {
                    room,
                    limits: config.limits,
                    admission: config.admission.clone(),
                    room_active: true,
                    shutdown_started: false,
                    idle_room_expiry: config.idle_room_expiry,
                    idle_generation: 0,
                },
                config.gateway_mailbox_capacity,
            )
            .map_err(|error| anyhow::anyhow!("register gateway: {error:?}"))?;

        let mut server_config = HttpServerConfig::dev();
        server_config.limits = tina_http::HttpLimits::default();
        server_config.service_call_timeout = Duration::from_secs(2);
        server_config.connection_mailbox_capacity = config.connection_mailbox_capacity;
        server_config.listener_mailbox_capacity = config.listener_mailbox_capacity;
        let listener = app
            .register_root::<_, Infallible>(
                HttpListener::<DemoShard, GatewayMsg>::with_config(
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
            .map_err(|error| anyhow::anyhow!("listener did not bind: {error:?}"))?;
        Ok(Self {
            addr,
            app,
            listener,
            gateway,
            room,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn report(&self) -> RoomReport {
        match self.app.call_blocking_request(
            self.room.requests,
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(SNAPSHOT_TICK)),
            Duration::from_secs(2),
        ) {
            Ok(CallOutcome::Replied(WebSocketSessionOutcome::Text(json))) => {
                parse_report_json(&json).unwrap_or_default()
            }
            _ => RoomReport::default(),
        }
    }

    pub fn wait_until(&self, timeout: Duration, f: impl Fn(&RoomReport) -> bool) -> RoomReport {
        let deadline = Instant::now() + timeout;
        loop {
            let report = self.report();
            if f(&report) || Instant::now() >= deadline {
                return report;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn shutdown_room(&self) -> anyhow::Result<RoomReport> {
        // Close gateway admission first so new upgrades see 503, then ask the
        // room to close members (gateway also forwards Shutdown on this path).
        let _ = self.app.call_blocking(
            self.gateway,
            GatewayMsg::Http(HttpRequest {
                method: Method::POST,
                path: GATEWAY_MARK_SHUTDOWN.to_string(),
                version: http::Version::HTTP_11,
                headers: http::HeaderMap::new(),
                body: tina_http::HttpRequestBody::Buffered(Vec::new()),
            }),
            Duration::from_secs(2),
        );
        let report = self.wait_until(Duration::from_secs(2), |r| {
            r.shutdown_started
                && r.shutdown_close_requested > 0
                && r.shutdown_close_ok + r.shutdown_close_failed >= r.shutdown_close_requested
        });
        if !(report.shutdown_started
            && report.shutdown_close_requested > 0
            && report.shutdown_close_ok + report.shutdown_close_failed
                >= report.shutdown_close_requested)
        {
            return Err(RoomShutdownTimeout { report }.into());
        }
        Ok(report)
    }

    pub fn note_client_a_received(&self) {
        let _ = self.app.try_send_event(
            self.room.events,
            WebSocketSessionMsg::Text(REPORT_CLIENT_A.to_string()),
        );
    }

    pub fn note_client_b_received(&self) {
        let _ = self.app.try_send_event(
            self.room.events,
            WebSocketSessionMsg::Text(REPORT_CLIENT_B.to_string()),
        );
    }

    pub fn stop(self) -> anyhow::Result<RoomReport> {
        let report = self.report();
        self.app
            .try_send(self.listener, HttpListenerMsg::Stop)
            .map_err(|error| anyhow::anyhow!("stop HTTP listener: {error:?}"))?;
        let handle = self.app.shutdown_handle();
        let _ = handle.request_and_wait_report(Duration::from_secs(5));
        drop(self.app);
        Ok(report)
    }
}

pub struct TlsRoomServer {
    addr: SocketAddr,
    app: LocalSystem<DemoShard, DefaultThreadedMailboxFactory>,
    listener: Address<HttpsListenerMsg, Result<HttpsReady, HttpsStartupError>>,
    room: RoomHandle,
    pub cert_der: Vec<u8>,
}

impl TlsRoomServer {
    pub fn start() -> anyhow::Result<Self> {
        Self::start_with(RoomServerConfig::default())
    }

    pub fn start_with(config: RoomServerConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.room_capacity == 1,
            "specimen_websocket_room supports one bounded named room"
        );
        let generated = generate_identity()?;
        let app = LocalSystem::single_shard(DemoShard, DefaultThreadedMailboxFactory)
            .config(LocalSystemConfig {
                ingress_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..LocalSystemConfig::default()
            })
            .try_build()?;

        let report = RoomReport {
            active_rooms: 1,
            room_capacity: config.room_capacity,
            member_capacity: config.member_capacity,
            room_high_water: 1,
            ..RoomReport::default()
        };

        let room = app
            .register_split_service::<
                Room,
                WebSocketSessionMsg,
                WebSocketSessionMsg,
                HttpConnectionMsg,
            >(
                Room {
                    members: BTreeMap::new(),
                    member_capacity: config.member_capacity,
                    idle_room_expiry: config.idle_room_expiry,
                    idle_generation: 0,
                    report,
                    first_closed: None,
                    stale_probe: None,
                    pending_stale_probe: None,
                    shutting_down: false,
                    deleting: false,
                },
                config.room_mailbox_capacity,
            )
            .map_err(|error| anyhow::anyhow!("register tls room: {error:?}"))?;
        let gateway = app
            .register_root::<Gateway, tina::ServiceMessage<WebSocketSessionMsg, WebSocketSessionMsg>>(
                Gateway {
                    room,
                    limits: config.limits,
                    admission: config.admission,
                    room_active: true,
                    shutdown_started: false,
                    idle_room_expiry: config.idle_room_expiry,
                    idle_generation: 0,
                },
                config.gateway_mailbox_capacity,
            )
            .map_err(|error| anyhow::anyhow!("register tls gateway: {error:?}"))?;
        let listener_isolate = HttpsListener::<DemoShard, GatewayMsg>::new(
            "127.0.0.1:0".parse().unwrap(),
            gateway,
            HttpsServerConfig::dev(generated.identity),
        );
        let listener = app
            .register_root::<HttpsListener<DemoShard, GatewayMsg>, Infallible>(
                listener_isolate,
                config.listener_mailbox_capacity,
            )
            .map_err(|error| anyhow::anyhow!("register https listener: {error:?}"))?;
        let ready = app
            .call_blocking(listener, HttpsListenerMsg::Start, Duration::from_secs(5))
            .map_err(|error| anyhow::anyhow!("call https listener: {error:?}"))?;
        let ready = match ready {
            CallOutcome::Replied(Ok(ready)) => ready,
            other => anyhow::bail!("https listener did not start: {other:?}"),
        };
        Ok(Self {
            addr: ready.local_addr,
            app,
            listener,
            room,
            cert_der: generated.cert_der,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn report(&self) -> RoomReport {
        match self.app.call_blocking_request(
            self.room.requests,
            WebSocketSessionMsg::AppControl(WebSocketSessionControl::Tick(SNAPSHOT_TICK)),
            Duration::from_secs(2),
        ) {
            Ok(CallOutcome::Replied(WebSocketSessionOutcome::Text(json))) => {
                parse_report_json(&json).unwrap_or_default()
            }
            _ => RoomReport::default(),
        }
    }

    pub fn wait_until(&self, timeout: Duration, f: impl Fn(&RoomReport) -> bool) -> RoomReport {
        let deadline = Instant::now() + timeout;
        loop {
            let report = self.report();
            if f(&report) || Instant::now() >= deadline {
                return report;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn stop(self) -> anyhow::Result<RoomReport> {
        let report = self.report();
        self.app
            .try_send(self.listener, HttpsListenerMsg::Stop)
            .map_err(|error| anyhow::anyhow!("stop HTTPS listener: {error:?}"))?;
        let handle = self.app.shutdown_handle();
        let _ = handle.request_and_wait_report(Duration::from_secs(5));
        drop(self.app);
        Ok(report)
    }
}

struct GeneratedIdentity {
    identity: TlsServerIdentity,
    cert_der: Vec<u8>,
}

fn generate_identity() -> anyhow::Result<GeneratedIdentity> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.signing_key.serialize_der();
    let identity = TlsServerIdentity::from_der(vec![cert_der.clone()], key_der);
    Ok(GeneratedIdentity { identity, cert_der })
}

pub fn run() -> anyhow::Result<RoomReport> {
    let server = RoomServer::start()?;
    run_script(&server);
    server.stop()
}

fn run_script(server: &RoomServer) {
    let mut a = Client::connect(server.addr);
    let mut b = Client::connect(server.addr);
    let _ = a.read_frame();
    let _ = b.read_frame();

    a.write_text("hello");
    if b.read_frame() == Some((0x1, b"room:hello".to_vec())) {
        server.note_client_b_received();
    }
    b.write_text("reply");
    if a.read_frame() == Some((0x1, b"room:reply".to_vec())) {
        server.note_client_a_received();
    }
    let _ = server.wait_until(Duration::from_secs(2), |r| r.broadcast_ok >= 2);

    b.write_close();
    let _ = b.read_frame();
    drop(b);

    let mut c = Client::connect(server.addr);
    let _ = c.read_frame();
    a.write_text(&"x".repeat(2000));
    let _ = server.wait_until(Duration::from_secs(2), |r| {
        r.broadcast_full > 0 || r.left > 0
    });

    let mut d = Client::connect(server.addr);
    let _ = d.read_frame();
    let _ = server.wait_until(Duration::from_secs(2), |r| r.stale_handle_rejected);
}

struct Client {
    stream: TcpStream,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let mut stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(
                b"GET /room HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            )
            .unwrap();
        read_head(&mut stream);
        Self { stream }
    }

    fn write_text(&mut self, text: &str) {
        self.stream
            .write_all(&masked_frame(0x1, text.as_bytes()))
            .unwrap();
        self.stream.flush().unwrap();
    }

    fn write_close(&mut self) {
        let _ = self.stream.write_all(&masked_frame(0x8, &[]));
        let _ = self.stream.flush();
    }

    fn read_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        read_frame(&mut self.stream)
    }
}

fn read_head(stream: &mut TcpStream) {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    assert!(String::from_utf8(head).unwrap().starts_with("HTTP/1.1 101"));
}

fn masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [1u8, 2, 3, 4];
    let mut out = vec![0x80 | opcode];
    if payload.len() < 126 {
        out.push(0x80 | payload.len() as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, byte) in payload.iter().enumerate() {
        out.push(*byte ^ mask[i % 4]);
    }
    out
}

fn read_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).ok()?;
    let opcode = head[0] & 0x0f;
    let len = usize::from(head[1] & 0x7f);
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).ok()?;
    Some((opcode, payload))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Duration;

    use tina_http::WebSocketLimits;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket, client, connect};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn connect_room(url: &str) -> WebSocket<MaybeTlsStream<TcpStream>> {
        let (mut ws, _) = connect(url).expect("connect room client");
        if let MaybeTlsStream::Plain(stream) = ws.get_mut() {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set room read timeout");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("set room write timeout");
        }
        ws
    }

    #[test]
    fn specimen_websocket_room_smoke() {
        let _guard = test_guard();
        let report = crate::run().expect("run room specimen");
        assert!(report.client_b_received, "{report:?}");
        assert!(report.broadcast_ok >= 1, "{report:?}");
        assert!(report.broadcast_full >= 1, "{report:?}");
        assert!(report.left >= 1, "{report:?}");
        assert!(report.joined >= 4, "{report:?}");
        assert!(report.refill_after_close, "{report:?}");
        assert!(report.stale_handle_rejected, "{report:?}");
    }

    #[test]
    fn invalid_room_capacity_is_returned_to_the_host() {
        let result = crate::RoomServer::start_with(crate::RoomServerConfig {
            room_capacity: 2,
            ..Default::default()
        });
        let error = match result {
            Ok(_) => panic!("unsupported room capacity must not start a runtime"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("supports one bounded named room"),
            "{error:#}"
        );
    }

    #[test]
    fn shutdown_timeout_is_typed_and_carries_the_last_snapshot() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            member_capacity: 0,
            ..Default::default()
        })
        .expect("start empty room server");

        let error = server
            .shutdown_room()
            .expect_err("an empty room cannot request a member close");
        let timeout = error
            .downcast_ref::<crate::RoomShutdownTimeout>()
            .expect("shutdown timeout remains typed");
        assert!(timeout.report.shutdown_started);
        assert_eq!(timeout.report.shutdown_close_requested, 0);
        assert_eq!(timeout.report, server.report());
        server.stop().expect("stop empty room server");
    }

    #[test]
    fn real_tungstenite_clients_use_the_room_and_report_endpoint() {
        let _guard = test_guard();
        let server = crate::RoomServer::start().expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut a = connect_room(url.as_str());
        let mut b = connect_room(url.as_str());

        assert!(a.read().expect("a join").is_text());
        assert!(b.read().expect("b join").is_text());

        a.send(Message::Text("from-a".into())).expect("send a");
        assert_eq!(
            b.read().expect("b broadcast").into_text().expect("text"),
            "room:from-a"
        );
        let report = server.wait_until(Duration::from_secs(2), |r| r.broadcast_ok >= 1);
        assert_eq!(report.live_members, 2, "{report:?}");
        assert!(http_get(server.addr(), "/room-report").contains("\"broadcast_ok\":1"));
        let _ = a.close(None);
        let _ = b.close(None);
        server.stop().expect("stop room server");
    }

    #[test]
    fn browser_client_page_is_served_and_points_at_room_websocket() {
        let _guard = test_guard();
        let server = crate::RoomServer::start().expect("start room server");
        let page = http_get(server.addr(), "/");
        assert!(page.contains("new WebSocket"), "{page}");
        assert!(page.contains("/room"), "{page}");
        assert!(page.contains("tina.room.v1"), "{page}");
        assert!(page.contains("location.protocol"), "{page}");
        server.stop().expect("stop room server");
    }

    #[test]
    fn admission_rejects_bad_origin_auth_and_subprotocol_then_accepts_good_headers() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            admission: crate::AdmissionPolicy {
                allowed_origin: Some("https://allowed.example".to_string()),
                required_bearer_token: Some("secret".to_string()),
                require_subprotocol: true,
            },
            ..Default::default()
        })
        .expect("start room server");

        let bad_origin = raw_upgrade(
            server.addr(),
            "Origin: https://evil.example\r\nAuthorization: Bearer secret\r\nSec-WebSocket-Protocol: tina.room.v1\r\n",
        );
        assert!(bad_origin.starts_with("HTTP/1.1 403"), "{bad_origin}");

        let bad_auth = raw_upgrade(
            server.addr(),
            "Origin: https://allowed.example\r\nAuthorization: Bearer wrong\r\nSec-WebSocket-Protocol: tina.room.v1\r\n",
        );
        assert!(bad_auth.starts_with("HTTP/1.1 401"), "{bad_auth}");

        let bad_subprotocol = raw_upgrade(
            server.addr(),
            "Origin: https://allowed.example\r\nAuthorization: Bearer secret\r\nSec-WebSocket-Protocol: other.v1\r\n",
        );
        assert!(
            bad_subprotocol.starts_with("HTTP/1.1 400"),
            "{bad_subprotocol}"
        );

        let ok = raw_upgrade(
            server.addr(),
            "Origin: https://allowed.example\r\nAuthorization: Bearer secret\r\nSec-WebSocket-Protocol: tina.room.v1, other.v1\r\n",
        );
        assert!(ok.starts_with("HTTP/1.1 101"), "{ok}");
        assert!(
            ok.to_ascii_lowercase()
                .contains("sec-websocket-protocol: tina.room.v1"),
            "{ok}"
        );

        let report = server.wait_until(Duration::from_secs(2), |r| {
            r.rejected_origin == 1 && r.rejected_auth == 1 && r.rejected_subprotocol == 1
        });
        assert_eq!(report.rejected_origin, 1, "{report:?}");
        assert_eq!(report.rejected_auth, 1, "{report:?}");
        assert_eq!(report.rejected_subprotocol, 1, "{report:?}");
        server.stop().expect("stop room server");
    }

    #[test]
    fn capacity_fill_close_refill_is_proven_with_real_clients() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            member_capacity: 2,
            ..Default::default()
        })
        .expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut a = connect_room(url.as_str());
        let mut b = connect_room(url.as_str());
        assert!(a.read().expect("a join").is_text());
        assert!(b.read().expect("b join").is_text());

        let mut over_capacity = connect_room(url.as_str());
        assert!(over_capacity.read().expect("room full close").is_close());
        let report = server.wait_until(Duration::from_secs(2), |r| r.rejected_full == 1);
        assert_eq!(report.live_members, 2, "{report:?}");

        let _ = a.close(None);
        assert!(a.read().expect("a close reply").is_close());
        let report = server.wait_until(Duration::from_secs(2), |r| r.live_members == 1);
        assert_eq!(report.left, 1, "{report:?}");

        let mut c = connect_room(url.as_str());
        assert!(c.read().expect("c join").is_text());
        let report = server.wait_until(Duration::from_secs(2), |r| r.refill_after_close);
        assert_eq!(report.live_members, 2, "{report:?}");
        let _ = b.close(None);
        let _ = c.close(None);
        server.stop().expect("stop room server");
    }

    #[test]
    fn session_report_request_is_bounded_and_visible() {
        let _guard = test_guard();
        let server = crate::RoomServer::start().expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut client = connect_room(url.as_str());
        assert!(client.read().expect("join").is_text());
        client
            .send(Message::Text("__report__".into()))
            .expect("request report");
        let report = server.wait_until(Duration::from_secs(2), |r| r.session_report_ok == 1);
        assert_eq!(report.session_report_ok, 1, "{report:?}");
        assert_eq!(report.queued_frame_high_water, 0, "{report:?}");
        let _ = client.close(None);
        server.stop().expect("stop room server");
    }

    #[test]
    fn slow_peer_pressure_removes_that_peer_and_other_clients_continue() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            limits: WebSocketLimits {
                max_queued_outbound_bytes: 256,
                ..Default::default()
            },
            member_capacity: 2,
            ..Default::default()
        })
        .expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut slow = connect_room(url.as_str());
        let mut sender = connect_room(url.as_str());
        assert!(slow.read().expect("slow join").is_text());
        assert!(sender.read().expect("sender join").is_text());

        sender
            .send(Message::Text("x".repeat(600).into()))
            .expect("send pressure payload");
        let report = server.wait_until(Duration::from_secs(2), |r| {
            r.broadcast_full >= 1 && r.slow_peer_closed >= 1 && r.live_members == 1
        });
        assert_eq!(report.live_members, 1, "{report:?}");

        let mut healthy = connect_room(url.as_str());
        assert!(healthy.read().expect("healthy join").is_text());
        let report = server.wait_until(Duration::from_secs(2), |r| r.live_members == 2);
        assert_eq!(report.live_members, 2, "{report:?}");

        sender
            .send(Message::Text("after-pressure".into()))
            .expect("send follow-up");
        assert_eq!(
            healthy
                .read()
                .expect("healthy still receives")
                .into_text()
                .expect("text"),
            "room:after-pressure"
        );
        let report = server.wait_until(Duration::from_secs(2), |r| r.broadcast_ok >= 1);
        assert_eq!(report.broadcast_ok, 1, "{report:?}");
        let _ = slow.close(None);
        let _ = sender.close(None);
        let _ = healthy.close(None);
        server.stop().expect("stop room server");
    }

    #[test]
    fn room_shutdown_closes_existing_clients_and_rejects_new_upgrades() {
        let _guard = test_guard();
        let server = crate::RoomServer::start().expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut client = connect_room(url.as_str());
        assert!(client.read().expect("join").is_text());

        let report = server.shutdown_room().expect("shut down room");
        assert!(report.shutdown_started, "{report:?}");
        assert_eq!(report.shutdown_close_requested, 1, "{report:?}");
        assert_eq!(report.shutdown_close_ok, 1, "{report:?}");
        assert!(client.read().expect("close").is_close());
        assert!(
            connect(url.as_str()).is_err(),
            "new upgrade after shutdown must fail"
        );
        server.stop().expect("stop room server");
    }

    #[test]
    fn room_delete_rejects_new_upgrades_then_create_allows_refill() {
        let _guard = test_guard();
        let server = crate::RoomServer::start().expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut client = connect_room(url.as_str());
        assert!(client.read().expect("join").is_text());

        let deleted = http_request(server.addr(), "DELETE", "/rooms/default");
        assert!(deleted.starts_with("HTTP/1.1 200"), "{deleted}");
        assert!(client.read().expect("delete close").is_close());
        let report = server.wait_until(Duration::from_secs(2), |r| {
            r.active_rooms == 0 && r.live_members == 0
        });
        assert_eq!(report.active_rooms, 0, "{report:?}");
        assert!(
            connect(url.as_str()).is_err(),
            "deleted room must reject upgrade"
        );

        let created = http_request(server.addr(), "POST", "/rooms/default");
        assert!(created.starts_with("HTTP/1.1 201"), "{created}");
        let mut refill = connect_room(url.as_str());
        assert!(refill.read().expect("refill join").is_text());
        let report = server.wait_until(Duration::from_secs(2), |r| r.active_rooms == 1);
        assert_eq!(report.active_rooms, 1, "{report:?}");
        let _ = refill.close(None);
        server.stop().expect("stop room server");
    }

    #[test]
    fn idle_room_expiry_rejects_until_room_is_created_again() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            idle_room_expiry: Duration::from_millis(20),
            ..Default::default()
        })
        .expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut before_idle = connect_room(url.as_str());
        assert!(before_idle.read().expect("join before idle").is_text());
        before_idle.close(None).expect("close before idle");
        assert!(before_idle.read().expect("close reply").is_close());
        let report = server.wait_until(Duration::from_secs(2), |r| r.active_rooms == 0);
        assert_eq!(report.active_rooms, 0, "{report:?}");

        assert!(
            connect(url.as_str()).is_err(),
            "expired room must reject upgrade"
        );
        let created = http_request(server.addr(), "POST", "/rooms/default");
        assert!(created.starts_with("HTTP/1.1 201"), "{created}");
        let mut client = connect_room(url.as_str());
        assert!(client.read().expect("join after idle create").is_text());
        let _ = client.close(None);
        server.stop().expect("stop room server");
    }

    #[test]
    fn shutdown_during_broadcast_closes_clients_without_hanging() {
        let _guard = test_guard();
        let server = crate::RoomServer::start().expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut a = connect_room(url.as_str());
        let mut b = connect_room(url.as_str());
        assert!(a.read().expect("a join").is_text());
        assert!(b.read().expect("b join").is_text());

        a.send(Message::Text("before-shutdown".into()))
            .expect("send before shutdown");
        assert_eq!(
            b.read()
                .expect("broadcast before shutdown")
                .into_text()
                .expect("text"),
            "room:before-shutdown"
        );

        let report = server.shutdown_room().expect("shut down room");
        assert_eq!(report.shutdown_close_requested, 2, "{report:?}");
        assert_eq!(
            report.shutdown_close_ok + report.shutdown_close_failed,
            2,
            "{report:?}"
        );
        assert!(
            connect(url.as_str()).is_err(),
            "post-shutdown connect must fail"
        );
        let _ = a.close(None);
        let _ = b.close(None);
        server.stop().expect("stop room server");
    }

    #[test]
    fn reconnect_refill_loop_does_not_leak_live_members() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            member_capacity: 1,
            ..Default::default()
        })
        .expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        for index in 0..25 {
            let mut client = connect_room(url.as_str());
            assert!(client.read().expect("join").is_text(), "index {index}");
            let report = server.wait_until(Duration::from_secs(2), |r| r.live_members == 1);
            assert_eq!(report.live_members, 1, "{report:?}");
            let _ = client.close(None);
            assert!(
                client.read().expect("close reply").is_close(),
                "index {index}"
            );
            let report = server.wait_until(Duration::from_secs(2), |r| r.live_members == 0);
            assert_eq!(report.live_members, 0, "{report:?}");
        }
        let report = server.report();
        assert_eq!(report.joined, 25, "{report:?}");
        assert_eq!(report.live_members, 0, "{report:?}");
        server.stop().expect("stop room server");
    }

    #[test]
    fn http_routes_coexist_with_websocket_activity() {
        let _guard = test_guard();
        let server = crate::RoomServer::start().expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut client = connect_room(url.as_str());
        assert!(client.read().expect("join").is_text());

        let page = http_get(server.addr(), "/");
        assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        let report = http_get(server.addr(), "/room-report");
        assert!(report.starts_with("HTTP/1.1 200"), "{report}");
        assert!(report.contains("\"live_members\":1"), "{report}");
        assert!(report.contains("\"active_rooms\":1"), "{report}");
        let health = http_get(server.addr(), "/health");
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");
        let ready = http_get(server.addr(), "/ready");
        assert!(ready.starts_with("HTTP/1.1 200"), "{ready}");
        let missing = http_get(server.addr(), "/missing");
        assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");

        let _ = client.close(None);
        server.stop().expect("stop room server");
    }

    #[test]
    fn real_tungstenite_client_works_over_wss() {
        let _guard = test_guard();
        let tls = crate::TlsRoomServer::start().expect("start tls");
        let tcp = TcpStream::connect_timeout(&tls.addr(), Duration::from_secs(2)).expect("tcp");
        tcp.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set tls read timeout");
        tcp.set_write_timeout(Some(Duration::from_secs(2)))
            .expect("set tls write timeout");
        let mut stream = rustls_client(tcp, tls.cert_der.clone());
        let request = format!("wss://localhost:{}/room", tls.addr().port());
        let (mut ws, _) = client(request.as_str(), &mut stream).expect("wss connect");
        assert!(ws.read().expect("join").is_text());
        ws.send(Message::Text("tls".into())).expect("send tls");
        let report = tls.wait_until(Duration::from_secs(2), |r| r.joined == 1);
        assert_eq!(report.live_members, 1, "{report:?}");
        let _ = ws.close(None);
        tls.stop().expect("stop TLS room server");
    }

    #[test]
    fn many_client_connect_send_shutdown_smoke_stays_bounded() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            member_capacity: 8,
            ..Default::default()
        })
        .expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut clients = Vec::new();
        for index in 0..8 {
            let mut client = connect_room(url.as_str());
            assert!(client.read().expect("join").is_text(), "index {index}");
            clients.push(client);
        }
        let report = server.wait_until(Duration::from_secs(2), |r| r.joined == 8);
        assert_eq!(report.joined, 8, "{report:?}");
        assert_eq!(report.live_members, 8, "{report:?}");

        let report = server.shutdown_room().expect("shut down room");
        assert_eq!(report.shutdown_close_requested, 8, "{report:?}");
        assert_eq!(
            report.shutdown_close_ok + report.shutdown_close_failed,
            8,
            "{report:?}"
        );
        for mut client in clients {
            let _ = client.close(None);
        }
        server.stop().expect("stop room server");
    }

    #[test]
    fn ci_short_load_churn_reports_high_water_and_shutdown() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            member_capacity: 6,
            ..Default::default()
        })
        .expect("start room server");
        let url = format!("ws://{}/room", server.addr());
        let mut clients = Vec::new();
        for index in 0..6 {
            let mut client = connect_room(url.as_str());
            assert!(client.read().expect("join").is_text(), "index {index}");
            clients.push(client);
        }
        for index in 0..12 {
            let mut churn = connect_room(url.as_str());
            assert!(
                churn.read().expect("capacity close").is_close(),
                "churn {index}"
            );
        }
        let report = server.wait_until(Duration::from_secs(2), |r| {
            r.joined == 6 && r.rejected_full == 12 && r.session_high_water == 6
        });
        assert_eq!(report.live_members, 6, "{report:?}");
        assert_eq!(report.session_high_water, 6, "{report:?}");
        assert_eq!(report.room_high_water, 1, "{report:?}");

        let report = server.shutdown_room().expect("shut down room");
        assert_eq!(report.shutdown_close_requested, 6, "{report:?}");
        assert_eq!(
            report.shutdown_close_ok + report.shutdown_close_failed,
            6,
            "{report:?}"
        );
        assert!(report.app_close_seen, "{report:?}");
        for mut client in clients {
            let _ = client.close(None);
        }
        let after = server.stop().expect("stop room server");
        assert_eq!(after.active_rooms, 1, "{after:?}");
    }

    #[test]
    fn fill_close_refill_member_capacity_shape() {
        let mut members = std::collections::BTreeSet::new();
        let capacity = 2;
        assert!(members.insert(1));
        assert!(members.insert(2));
        assert_eq!(members.len(), capacity);
        assert!(members.remove(&1));
        assert!(members.insert(3));
        assert_eq!(members.len(), capacity);
        assert!(!members.contains(&1));
        assert!(members.contains(&3));
    }

    fn rustls_client(
        tcp: TcpStream,
        root_cert_der: Vec<u8>,
    ) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(root_cert_der))
            .expect("root cert");
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_name =
            rustls::pki_types::ServerName::try_from("localhost").expect("server name");
        let connection = rustls::ClientConnection::new(Arc::new(config), server_name)
            .expect("client connection");
        rustls::StreamOwned::new(connection, tcp)
    }

    fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
        http_request(addr, "GET", path)
    }

    fn http_request(addr: std::net::SocketAddr, method: &str, path: &str) -> String {
        let mut stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .expect("write request");
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).expect("read response");
        String::from_utf8(bytes).expect("utf8 response")
    }

    fn raw_upgrade(addr: std::net::SocketAddr, extra_headers: &str) -> String {
        let mut stream =
            TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write!(
            stream,
            "GET /room HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{extra_headers}\r\n"
        )
        .expect("write upgrade");
        let mut bytes = Vec::new();
        let mut one = [0u8; 1];
        while !bytes.ends_with(b"\r\n\r\n") {
            if stream.read_exact(&mut one).is_err() {
                break;
            }
            bytes.push(one[0]);
        }
        String::from_utf8(bytes).expect("utf8 upgrade")
    }
}
