use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use http::Method;
use tina::CallContext;
use tina::prelude::*;
use tina_http::{
    HttpListener, HttpListenerMsg, HttpsListener, HttpsListenerMsg, HttpsReady, HttpsServerConfig,
    HttpsStartupError, HttpRequest, HttpResponse, TlsServerIdentity, WebSocketCloseCode,
    WebSocketError, WebSocketLimits, WebSocketSendError, WebSocketSessionHandle,
    WebSocketSessionId, WebSocketSessionMsg, WebSocketSessionOutcome, websocket_upgrade,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeConfig, sleep,
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
            "{{\"active_rooms\":{},\"room_capacity\":{},\"member_capacity\":{},\"joined\":{},\"left\":{},\"rejected_origin\":{},\"rejected_auth\":{},\"rejected_subprotocol\":{},\"rejected_full\":{},\"rejected_shutdown\":{},\"broadcast_ok\":{},\"broadcast_full\":{},\"broadcast_closed\":{},\"broadcast_timeout\":{},\"slow_peer_closed\":{},\"live_members\":{},\"shutdown_started\":{},\"shutdown_close_requested\":{},\"shutdown_close_ok\":{},\"shutdown_close_failed\":{},\"stale_handle_rejected\":{},\"refill_after_close\":{},\"selected_subprotocol_seen\":{},\"session_report_ok\":{},\"session_report_stale\":{},\"session_high_water\":{},\"room_high_water\":{},\"queued_frame_high_water\":{},\"queued_byte_high_water\":{},\"app_close_seen\":{},\"peer_close_seen\":{},\"protocol_close_seen\":{},\"timeout_close_seen\":{},\"client_a_received\":{},\"client_b_received\":{}}}",
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

#[derive(Debug, Default)]
struct SharedReport {
    report: Mutex<RoomReport>,
    changed: Condvar,
}

impl SharedReport {
    fn update(&self, f: impl FnOnce(&mut RoomReport)) {
        let mut report = self.report.lock().expect("report lock");
        f(&mut report);
        self.changed.notify_all();
    }

    fn snapshot(&self) -> RoomReport {
        self.report.lock().expect("report lock").clone()
    }

    fn wait_until(&self, timeout: Duration, f: impl Fn(&RoomReport) -> bool) -> RoomReport {
        let deadline = Instant::now() + timeout;
        let mut report = self.report.lock().expect("report lock");
        while !f(&report) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next, _) = self
                .changed
                .wait_timeout(report, remaining)
                .expect("report wait");
            report = next;
        }
        report.clone()
    }
}

#[derive(Debug, Default)]
struct DemoShard;

impl Shard for DemoShard {
    fn id(&self) -> ShardId {
        ShardId::new(987)
    }
}

#[derive(Debug)]
struct Gateway {
    room: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
    limits: WebSocketLimits,
    admission: AdmissionPolicy,
    report: Arc<SharedReport>,
    room_active: bool,
    idle_room_expiry: Duration,
    idle_generation: u64,
}

impl Isolate for Gateway {
    tina::isolate_types! {
        message: HttpRequest,
        reply: HttpResponse,
        send: tina::Outbound<WebSocketSessionMsg>,
        spawn: Infallible,
        call: RuntimeCall<HttpRequest>,
        shard: DemoShard,
    }

    fn handle(
        &mut self,
        request: HttpRequest,
        _ctx: &mut Context<'_, DemoShard, Self::Reply>,
    ) -> Effect<Self> {
        if let Some(generation) = idle_generation_from_path(&request.path) {
            return self.handle_idle_deadline(generation);
        }
        let is_delete = request.method == Method::DELETE && request.path == "/rooms/default";
        let is_create = request.method == Method::POST && request.path == "/rooms/default";
        let response = self.response_for(request);
        if is_delete {
            batch(vec![reply(response), self.room_delete_effect()])
        } else if is_create {
            batch(vec![reply(response), self.room_create_effect()])
        } else {
            reply(response)
        }
    }

    fn handle_call(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        if let Some(generation) = idle_generation_from_path(&request.path) {
            return batch(vec![
                self.handle_idle_deadline(generation),
                call.reply(HttpResponse::with_body(http::StatusCode::OK, Vec::new())),
            ]);
        }
        let is_delete = request.method == Method::DELETE && request.path == "/rooms/default";
        let is_create = request.method == Method::POST && request.path == "/rooms/default";
        let response = self.response_for(request);
        let mut effects = vec![call.reply(response), self.arm_idle_deadline()];
        if is_delete {
            effects.push(self.room_delete_effect());
        } else if is_create {
            effects.push(self.room_create_effect());
        }
        batch(effects)
    }
}

impl Gateway {
    fn response_for(&mut self, request: HttpRequest) -> HttpResponse {
        if request.method == Method::GET && request.path == "/room" {
            if !self.room_active || self.report.snapshot().active_rooms == 0 {
                self.report.update(|r| r.rejected_shutdown += 1);
                return HttpResponse::service_unavailable();
            }
            if !self.admission.origin_allowed(&request) {
                self.report.update(|r| r.rejected_origin += 1);
                return HttpResponse::with_body(http::StatusCode::FORBIDDEN, b"bad origin".to_vec());
            }
            if !self.admission.auth_allowed(&request) {
                self.report.update(|r| r.rejected_auth += 1);
                return HttpResponse::with_body(
                    http::StatusCode::UNAUTHORIZED,
                    b"bad auth".to_vec(),
                );
            }
            if self.report.snapshot().shutdown_started {
                self.report.update(|r| r.rejected_shutdown += 1);
                return HttpResponse::service_unavailable();
            }
            match websocket_upgrade(&request, self.limits) {
                Ok(upgrade)
                    if upgrade
                        .offered_subprotocols()
                        .iter()
                        .any(|protocol| protocol == "tina.room.v1") =>
                {
                    match upgrade.accept_subprotocol(self.room, self.limits, "tina.room.v1") {
                        Ok(accept) => HttpResponse::websocket(accept),
                        Err(_) => HttpResponse::bad_request(),
                    }
                }
                Ok(_) if self.admission.require_subprotocol => {
                    self.report.update(|r| r.rejected_subprotocol += 1);
                    HttpResponse::bad_request()
                }
                Ok(upgrade) => HttpResponse::websocket(upgrade.accept(self.room, self.limits)),
                Err(_) => HttpResponse::bad_request(),
            }
        } else if request.method == Method::POST && request.path == "/rooms/default" {
            self.room_active = true;
            self.report.update(|r| {
                r.active_rooms = 1;
                r.room_high_water = r.room_high_water.max(1);
                r.shutdown_started = false;
            });
            HttpResponse::with_body(http::StatusCode::CREATED, b"created".to_vec())
        } else if request.method == Method::DELETE && request.path == "/rooms/default" {
            self.delete_room();
            HttpResponse::with_body(http::StatusCode::OK, b"deleted".to_vec())
        } else if request.method == Method::GET && request.path == "/" {
            let mut response =
                HttpResponse::with_body(http::StatusCode::OK, BROWSER_CLIENT.as_bytes().to_vec());
            response.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/html; charset=utf-8"),
            );
            response
        } else if request.method == Method::GET && request.path == "/room-report" {
            let mut response = HttpResponse::with_body(
                http::StatusCode::OK,
                self.report.snapshot().to_json().into_bytes(),
            );
            response.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            response
        } else if request.method == Method::GET && request.path == "/health" {
            HttpResponse::with_body(http::StatusCode::OK, b"healthy".to_vec())
        } else if request.method == Method::GET && request.path == "/ready" {
            let report = self.report.snapshot();
            if report.shutdown_started {
                HttpResponse::service_unavailable()
            } else {
                HttpResponse::with_body(http::StatusCode::OK, b"accepting".to_vec())
            }
        } else {
            HttpResponse::not_found()
        }
    }

    fn delete_room(&mut self) {
        if !self.room_active {
            return;
        }
        self.room_active = false;
        self.report.update(|r| {
            r.active_rooms = 0;
            r.shutdown_started = true;
        });
    }

    fn room_delete_effect(&self) -> Effect<Self> {
        send(
            self.room,
            WebSocketSessionMsg::Shutdown {
                code: Some(WebSocketCloseCode(1001)),
                reason: b"room delete".to_vec(),
            },
        )
    }

    fn room_create_effect(&self) -> Effect<Self> {
        send(
            self.room,
            WebSocketSessionMsg::Text(ROOM_CREATE_CONTROL.to_string()),
        )
    }

    fn arm_idle_deadline(&mut self) -> Effect<Self> {
        if !self.room_active || self.idle_room_expiry.is_zero() {
            return noop();
        }
        self.idle_generation = self.idle_generation.saturating_add(1);
        let generation = self.idle_generation;
        sleep(self.idle_room_expiry).then(move |_| idle_deadline_request(generation))
    }

    fn handle_idle_deadline(&mut self, generation: u64) -> Effect<Self> {
        if generation == self.idle_generation
            && self.room_active
            && self.report.snapshot().live_members == 0
        {
            self.delete_room();
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

const BROWSER_CLIENT: &str = include_str!("../browser_client.html");
const ROOM_CREATE_CONTROL: &str = "__room_create__";
const ROOM_IDLE_EXPIRE_CONTROL: &str = "__room_idle_expire__:";

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

#[derive(Debug)]
struct Room {
    members: BTreeMap<WebSocketSessionId, WebSocketSessionHandle>,
    member_capacity: usize,
    idle_room_expiry: Duration,
    idle_generation: u64,
    report: Arc<SharedReport>,
    first_closed: Option<WebSocketSessionId>,
    stale_probe: Option<WebSocketSessionHandle>,
    pending_stale_probe: Option<WebSocketSessionId>,
    shutting_down: bool,
    deleting: bool,
}

impl Isolate for Room {
    tina::isolate_types! {
        message: WebSocketSessionMsg,
        reply: WebSocketSessionOutcome,
        send: tina::Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<WebSocketSessionMsg>,
        shard: DemoShard,
    }

    fn handle(
        &mut self,
        msg: WebSocketSessionMsg,
        _ctx: &mut Context<'_, DemoShard, Self::Reply>,
    ) -> Effect<Self> {
        self.handle_room_msg(msg)
    }

    fn handle_call(
        &mut self,
        msg: WebSocketSessionMsg,
        _call: CallContext<'_, Self>,
    ) -> Effect<Self> {
        self.handle_room_msg(msg)
    }
}

impl Room {
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
        sleep(self.idle_room_expiry).then(move |_| {
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
        self.report.update(|r| {
            r.active_rooms = 0;
            r.shutdown_started = true;
        });
        true
    }

    fn handle_room_msg(&mut self, msg: WebSocketSessionMsg) -> Effect<Self> {
        match msg {
            WebSocketSessionMsg::SessionOpen { session } => {
                self.idle_generation = self.idle_generation.saturating_add(1);
                let session_id = session.session_id();
                if self.shutting_down {
                    self.report.update(|r| r.rejected_shutdown += 1);
                    reply(WebSocketSessionOutcome::Close(
                        Some(WebSocketCloseCode(1001)),
                        b"room shutting down".to_vec(),
                    ))
                } else if self.members.len() < self.member_capacity {
                    let mut effects = Vec::new();
                    if self.first_closed.is_some() {
                        self.report.update(|r| r.refill_after_close = true);
                        if self.pending_stale_probe.is_none()
                            && let Some(stale) = self.stale_probe.take()
                        {
                            let stale_session = stale.session_id();
                            self.pending_stale_probe = Some(stale_session);
                            effects
                                .push(stale.text_effect::<Room>("stale-proof", Duration::from_secs(1)));
                        }
                    }
                    self.members.insert(session_id, session);
                    self.report.update(|r| {
                        r.joined += 1;
                        r.live_members = self.members.len();
                        r.session_high_water = r.session_high_water.max(self.members.len());
                    });
                    effects.push(reply(WebSocketSessionOutcome::Text(format!(
                        "join:{}",
                        session_id.raw()
                    ))));
                    batch(effects)
                } else {
                    self.report.update(|r| r.rejected_full += 1);
                    reply(WebSocketSessionOutcome::Close(
                        Some(WebSocketCloseCode(1013)),
                        b"room full".to_vec(),
                    ))
                }
            }
            WebSocketSessionMsg::SessionText { session_id, text } => {
                if text == "__report__"
                    && let Some(handle) = self.members.get(&session_id).copied()
                {
                    return batch(vec![
                        handle.report_effect::<Room>(Duration::from_secs(1)),
                        reply(WebSocketSessionOutcome::None),
                    ]);
                }
                let mut effects = Vec::new();
                for (target_id, handle) in self.members.iter() {
                    if *target_id != session_id {
                        effects.push(handle.text_effect::<Room>(
                            format!("room:{text}"),
                            Duration::from_secs(1),
                        ));
                    }
                }
                if effects.is_empty() {
                    reply(WebSocketSessionOutcome::None)
                } else {
                    effects.push(reply(WebSocketSessionOutcome::None));
                    batch(effects)
                }
            }
            WebSocketSessionMsg::SendOutcome(outcome) => {
                let stale_probe = self.pending_stale_probe == Some(outcome.session);
                if stale_probe {
                    self.pending_stale_probe = None;
                }
                self.report.update(|r| match outcome.result {
                    Ok(()) => {
                        if self.shutting_down {
                            r.shutdown_close_ok += 1;
                            r.app_close_seen = true;
                        } else {
                            r.broadcast_ok += 1;
                        }
                    }
                    Err(WebSocketSendError::OutboundQueueFull | WebSocketSendError::OutboundBytesFull) => {
                        if self.shutting_down {
                            r.shutdown_close_failed += 1;
                        } else if stale_probe {
                            r.stale_handle_rejected = true;
                        } else {
                            r.broadcast_full += 1;
                            r.slow_peer_closed += 1;
                        }
                    }
                    Err(WebSocketSendError::Closed | WebSocketSendError::Stale) => {
                        if self.shutting_down {
                            r.shutdown_close_failed += 1;
                        } else if stale_probe || matches!(outcome.result, Err(WebSocketSendError::Stale)) {
                            r.stale_handle_rejected = true;
                        } else {
                            r.broadcast_closed += 1;
                        }
                    }
                    Err(WebSocketSendError::Closing) => {
                        if self.shutting_down {
                            r.shutdown_close_failed += 1;
                        } else if stale_probe {
                            r.stale_handle_rejected = true;
                        } else {
                            r.broadcast_closed += 1;
                        }
                    }
                    Err(WebSocketSendError::Protocol) => {
                        if self.shutting_down {
                            r.shutdown_close_failed += 1;
                        } else if stale_probe {
                            r.stale_handle_rejected = true;
                        } else {
                            r.broadcast_closed += 1;
                        }
                    }
                    Err(WebSocketSendError::Timeout) => {
                        if self.shutting_down {
                            r.shutdown_close_failed += 1;
                        } else if stale_probe {
                            r.stale_handle_rejected = true;
                        } else {
                            r.broadcast_timeout += 1;
                        }
                    }
                });
                let mut removed_member = false;
                if outcome.result.is_err() && !stale_probe {
                    if let Some(handle) = self.members.remove(&outcome.session)
                        && self.stale_probe.is_none()
                    {
                        self.stale_probe = Some(handle);
                    }
                    self.first_closed = Some(outcome.session);
                    removed_member = true;
                    self.report.update(|r| {
                        r.left += 1;
                        r.live_members = self.members.len();
                    });
                }
                self.after_possible_leave(removed_member)
            }
            WebSocketSessionMsg::SessionReport(outcome) => {
                self.report.update(|r| match outcome.result {
                    Ok(report) => {
                        r.session_report_ok += 1;
                        r.queued_frame_high_water =
                            r.queued_frame_high_water.max(report.queued_outbound_frames);
                        r.queued_byte_high_water =
                            r.queued_byte_high_water.max(report.queued_outbound_bytes);
                    }
                    Err(WebSocketSendError::Stale) => {
                        r.session_report_stale += 1;
                        r.stale_handle_rejected = true;
                    }
                    Err(_) => {}
                });
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Shutdown { code, reason } => {
                let room_delete = reason.as_slice() == b"room delete";
                self.shutting_down = true;
                self.deleting = room_delete;
                let effects = self
                    .members
                    .values()
                    .copied()
                    .map(|handle| {
                        handle.close_effect::<Room>(code, reason.clone(), Duration::from_secs(1))
                    })
                    .collect::<Vec<_>>();
                self.report.update(|r| {
                    r.shutdown_started = true;
                    r.shutdown_close_requested += effects.len();
                });
                if effects.is_empty() {
                    if self.deleting {
                        self.shutting_down = false;
                        self.deleting = false;
                    }
                    reply(WebSocketSessionOutcome::None)
                } else {
                    batch(effects)
                }
            }
            WebSocketSessionMsg::SessionClose { session_id, .. }
            | WebSocketSessionMsg::SessionClosed { session_id, .. } => {
                let mut removed_member = false;
                if let Some(handle) = self.members.remove(&session_id)
                    && self.stale_probe.is_none()
                {
                    self.stale_probe = Some(handle);
                    removed_member = true;
                }
                self.first_closed = Some(session_id);
                self.report.update(|r| {
                    r.left += 1;
                    r.live_members = self.members.len();
                    r.peer_close_seen = true;
                });
                if self.deleting && self.members.is_empty() {
                    self.shutting_down = false;
                    self.deleting = false;
                }
                self.after_possible_leave(removed_member)
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
                self.report.update(|r| {
                    r.left += 1;
                    r.live_members = self.members.len();
                    match error {
                        WebSocketError::Timeout => r.timeout_close_seen = true,
                        WebSocketError::ProtocolError
                        | WebSocketError::InvalidClosePayload
                        | WebSocketError::ClientFrameUnmasked
                        | WebSocketError::InvalidOpcode(_) => r.protocol_close_seen = true,
                        _ => {}
                    }
                });
                if self.deleting && self.members.is_empty() {
                    self.shutting_down = false;
                    self.deleting = false;
                }
                self.after_possible_leave(removed_member)
            }
            WebSocketSessionMsg::SessionPressure { error, .. } => {
                self.report.update(|r| match error {
                    WebSocketError::Timeout => r.timeout_close_seen = true,
                    WebSocketError::ProtocolError
                    | WebSocketError::InvalidClosePayload
                    | WebSocketError::ClientFrameUnmasked
                    | WebSocketError::InvalidOpcode(_) => r.protocol_close_seen = true,
                    _ => {}
                });
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Closed(WebSocketError::PeerClosed)
            | WebSocketSessionMsg::Closed(WebSocketError::Closing)
            | WebSocketSessionMsg::Pressure(WebSocketError::PeerClosed) => {
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Close(_, _) => {
                self.report.update(|r| r.peer_close_seen = true);
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::Text(text) if text == ROOM_CREATE_CONTROL => {
                self.shutting_down = false;
                self.deleting = false;
                self.idle_generation = self.idle_generation.saturating_add(1);
                self.report.update(|r| {
                    r.active_rooms = 1;
                    r.shutdown_started = false;
                });
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
            WebSocketSessionMsg::Open
            | WebSocketSessionMsg::Text(_)
            | WebSocketSessionMsg::Ping(_)
            | WebSocketSessionMsg::Pong(_)
            | WebSocketSessionMsg::Pressure(_)
            | WebSocketSessionMsg::Closed(_)
            | WebSocketSessionMsg::AppControl(_) => reply(WebSocketSessionOutcome::None),
            WebSocketSessionMsg::SessionAccepted {
                selected_subprotocol,
                ..
            } => {
                if selected_subprotocol.as_deref() == Some("tina.room.v1") {
                    self.report.update(|r| r.selected_subprotocol_seen = true);
                }
                reply(WebSocketSessionOutcome::None)
            }
            WebSocketSessionMsg::SessionBinary { bytes, .. }
            | WebSocketSessionMsg::Binary(bytes) => reply(WebSocketSessionOutcome::Binary(bytes)),
        }
    }
}

pub struct RoomServer {
    addr: SocketAddr,
    runtime: ThreadedRuntime<DemoShard, DefaultThreadedMailboxFactory>,
    listener: tina_http::HttpListenerAddress,
    room: Address<WebSocketSessionMsg, WebSocketSessionOutcome>,
    report: Arc<SharedReport>,
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
    pub fn start() -> Self {
        Self::start_with(RoomServerConfig::default())
    }

    pub fn start_with(config: RoomServerConfig) -> Self {
        assert_eq!(
            config.room_capacity, 1,
            "specimen_websocket_room supports one bounded named room"
        );
        let report = Arc::new(SharedReport::default());
        report.update(|r| {
            r.active_rooms = 1;
            r.room_capacity = config.room_capacity;
            r.member_capacity = config.member_capacity;
            r.room_high_water = 1;
        });
        let runtime = ThreadedRuntime::with_config(
            DemoShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );
        let room = runtime
            .register_with_capacity::<Room, Infallible>(
                Room {
                    members: BTreeMap::new(),
                    member_capacity: config.member_capacity,
                    idle_room_expiry: config.idle_room_expiry,
                    idle_generation: 0,
                    report: report.clone(),
                    first_closed: None,
                    stale_probe: None,
                    pending_stale_probe: None,
                    shutting_down: false,
                    deleting: false,
                },
                config.room_mailbox_capacity,
            )
            .expect("register room");
        let gateway = runtime
            .register_with_capacity::<Gateway, WebSocketSessionMsg>(
                Gateway {
                    room,
                    limits: config.limits,
                    admission: config.admission.clone(),
                    report: report.clone(),
                    room_active: true,
                    idle_room_expiry: config.idle_room_expiry,
                    idle_generation: 0,
                },
                config.gateway_mailbox_capacity,
            )
            .expect("register gateway");
        let listener_isolate = HttpListener::<DemoShard>::new(
            "127.0.0.1:0".parse().unwrap(),
            gateway,
            tina_http::HttpLimits::default(),
            Duration::from_secs(2),
            config.connection_mailbox_capacity,
        );
        let listener = runtime
            .register_with_capacity::<HttpListener<DemoShard>, Infallible>(
                listener_isolate,
                config.listener_mailbox_capacity,
            )
            .expect("register listener");
        let bound = runtime.observe_next_bound();
        runtime
            .try_send(listener, HttpListenerMsg::Start)
            .expect("start listener");
        let addr = bound.wait(Duration::from_secs(2)).expect("bound address");
        Self {
            addr,
            runtime,
            listener,
            room,
            report,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn report(&self) -> RoomReport {
        self.report.snapshot()
    }

    pub fn wait_until(&self, timeout: Duration, f: impl Fn(&RoomReport) -> bool) -> RoomReport {
        self.report.wait_until(timeout, f)
    }

    pub fn shutdown_room(&self) -> RoomReport {
        self.runtime
            .try_send(
                self.room,
                WebSocketSessionMsg::Shutdown {
                    code: Some(WebSocketCloseCode(1001)),
                    reason: b"server shutdown".to_vec(),
                },
            )
            .expect("send room shutdown");
        self.wait_until(Duration::from_secs(2), |r| {
            r.shutdown_started
                && r.shutdown_close_requested > 0
                && r.shutdown_close_ok + r.shutdown_close_failed >= r.shutdown_close_requested
        })
    }

    pub fn stop(self) -> RoomReport {
        let report = self.report();
        let _ = self.runtime.try_send(self.listener, HttpListenerMsg::Stop);
        let _ = self.runtime.shutdown();
        report
    }
}

pub struct TlsRoomServer {
    addr: SocketAddr,
    runtime: ThreadedRuntime<DemoShard, DefaultThreadedMailboxFactory>,
    listener: Address<HttpsListenerMsg, Result<HttpsReady, HttpsStartupError>>,
    report: Arc<SharedReport>,
}

impl TlsRoomServer {
    pub fn start() -> Self {
        Self::start_with(RoomServerConfig::default())
    }

    pub fn start_with(config: RoomServerConfig) -> Self {
        assert_eq!(
            config.room_capacity, 1,
            "specimen_websocket_room supports one bounded named room"
        );
        let generated = generate_identity();
        let report = Arc::new(SharedReport::default());
        report.update(|r| {
            r.active_rooms = 1;
            r.room_capacity = config.room_capacity;
            r.member_capacity = config.member_capacity;
            r.room_high_water = 1;
        });
        let runtime = ThreadedRuntime::with_config(
            DemoShard,
            DefaultThreadedMailboxFactory,
            ThreadedRuntimeConfig {
                command_capacity: 64,
                idle_wait: Duration::from_millis(1),
                ..Default::default()
            },
        );
        let room = runtime
            .register_with_capacity::<Room, Infallible>(
                Room {
                    members: BTreeMap::new(),
                    member_capacity: config.member_capacity,
                    idle_room_expiry: config.idle_room_expiry,
                    idle_generation: 0,
                    report: report.clone(),
                    first_closed: None,
                    stale_probe: None,
                    pending_stale_probe: None,
                    shutting_down: false,
                    deleting: false,
                },
                config.room_mailbox_capacity,
            )
            .expect("register tls room");
        let gateway = runtime
            .register_with_capacity::<Gateway, WebSocketSessionMsg>(
                Gateway {
                    room,
                    limits: config.limits,
                    admission: config.admission,
                    report: report.clone(),
                    room_active: true,
                    idle_room_expiry: config.idle_room_expiry,
                    idle_generation: 0,
                },
                config.gateway_mailbox_capacity,
            )
            .expect("register tls gateway");
        let listener_isolate = HttpsListener::<DemoShard>::new(
            "127.0.0.1:0".parse().unwrap(),
            gateway,
            HttpsServerConfig::dev(generated.identity),
        );
        let listener = runtime
            .register_with_capacity::<HttpsListener<DemoShard>, Infallible>(
                listener_isolate,
                config.listener_mailbox_capacity,
            )
            .expect("register https listener");
        let ready = runtime
            .call_blocking(listener, HttpsListenerMsg::Start, Duration::from_secs(5))
            .expect("start https");
        let ready = match ready {
            CallOutcome::Replied(Ok(ready)) => ready,
            other => panic!("https listener did not start: {other:?}"),
        };
        Self {
            addr: ready.local_addr,
            runtime,
            listener,
            report,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn report(&self) -> RoomReport {
        self.report.snapshot()
    }

    pub fn stop(self) -> RoomReport {
        let report = self.report();
        let _ = self.runtime.try_send(self.listener, HttpsListenerMsg::Stop);
        let _ = self.runtime.shutdown();
        report
    }
}

struct GeneratedIdentity {
    identity: TlsServerIdentity,
}

fn generate_identity() -> GeneratedIdentity {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen self-sign");
    let cert_der = certified.cert.der().to_vec();
    let key_der = certified.signing_key.serialize_der();
    let identity = TlsServerIdentity::from_der(vec![cert_der], key_der);
    GeneratedIdentity { identity }
}

pub fn run() -> RoomReport {
    let server = RoomServer::start();
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
        server.report.update(|r| r.client_b_received = true);
    }
    b.write_text("reply");
    if a.read_frame() == Some((0x1, b"room:reply".to_vec())) {
        server.report.update(|r| r.client_a_received = true);
    }
    let _ = server.wait_until(Duration::from_secs(2), |r| r.broadcast_ok >= 2);

    b.write_close();
    let _ = b.read_frame();
    drop(b);

    let mut c = Client::connect(server.addr);
    let _ = c.read_frame();
    a.write_text(&"x".repeat(2000));
    let _ = server.wait_until(Duration::from_secs(2), |r| r.broadcast_full > 0 || r.left > 0);

    let mut d = Client::connect(server.addr);
    let _ = d.read_frame();
    let _ = server.wait_until(Duration::from_secs(2), |r| r.stale_handle_rejected);
}

struct Client {
    stream: TcpStream,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        stream
            .write_all(
                b"GET /room HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            )
            .unwrap();
        read_head(&mut stream);
        Self { stream }
    }

    fn write_text(&mut self, text: &str) {
        self.stream.write_all(&masked_frame(0x1, text.as_bytes())).unwrap();
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
    use std::convert::Infallible;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Duration;

    use tina_http::{
        HttpsListener, HttpsListenerMsg, HttpsServerConfig, TlsServerIdentity, WebSocketLimits,
    };
    use tina_runtime::{
        CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig,
    };
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket, client, connect};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
        let report = crate::run();
        assert!(report.client_b_received, "{report:?}");
        assert!(report.broadcast_ok >= 1, "{report:?}");
        assert!(report.broadcast_full >= 1, "{report:?}");
        assert!(report.left >= 1, "{report:?}");
        assert!(report.joined >= 4, "{report:?}");
        assert!(report.refill_after_close, "{report:?}");
        assert!(report.stale_handle_rejected, "{report:?}");
    }

    #[test]
    fn real_tungstenite_clients_use_the_room_and_report_endpoint() {
        let _guard = test_guard();
        let server = crate::RoomServer::start();
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
        let _ = server.stop();
    }

    #[test]
    fn browser_client_page_is_served_and_points_at_room_websocket() {
        let _guard = test_guard();
        let server = crate::RoomServer::start();
        let page = http_get(server.addr(), "/");
        assert!(page.contains("new WebSocket"), "{page}");
        assert!(page.contains("/room"), "{page}");
        assert!(page.contains("tina.room.v1"), "{page}");
        assert!(page.contains("location.protocol"), "{page}");
        let _ = server.stop();
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
        });

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
        assert!(bad_subprotocol.starts_with("HTTP/1.1 400"), "{bad_subprotocol}");

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
        let _ = server.stop();
    }

    #[test]
    fn capacity_fill_close_refill_is_proven_with_real_clients() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            member_capacity: 2,
            ..Default::default()
        });
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
        let _ = server.stop();
    }

    #[test]
    fn session_report_request_is_bounded_and_visible() {
        let _guard = test_guard();
        let server = crate::RoomServer::start();
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
        let _ = server.stop();
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
        });
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
        let _ = server.stop();
    }

    #[test]
    fn room_shutdown_closes_existing_clients_and_rejects_new_upgrades() {
        let _guard = test_guard();
        let server = crate::RoomServer::start();
        let url = format!("ws://{}/room", server.addr());
        let mut client = connect_room(url.as_str());
        assert!(client.read().expect("join").is_text());

        let report = server.shutdown_room();
        assert!(report.shutdown_started, "{report:?}");
        assert_eq!(report.shutdown_close_requested, 1, "{report:?}");
        assert_eq!(report.shutdown_close_ok, 1, "{report:?}");
        assert!(client.read().expect("close").is_close());
        assert!(connect(url.as_str()).is_err(), "new upgrade after shutdown must fail");
        let _ = server.stop();
    }

    #[test]
    fn room_delete_rejects_new_upgrades_then_create_allows_refill() {
        let _guard = test_guard();
        let server = crate::RoomServer::start();
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
        assert!(connect(url.as_str()).is_err(), "deleted room must reject upgrade");

        let created = http_request(server.addr(), "POST", "/rooms/default");
        assert!(created.starts_with("HTTP/1.1 201"), "{created}");
        let mut refill = connect_room(url.as_str());
        assert!(refill.read().expect("refill join").is_text());
        let report = server.wait_until(Duration::from_secs(2), |r| r.active_rooms == 1);
        assert_eq!(report.active_rooms, 1, "{report:?}");
        let _ = refill.close(None);
        let _ = server.stop();
    }

    #[test]
    fn idle_room_expiry_rejects_until_room_is_created_again() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            idle_room_expiry: Duration::from_millis(20),
            ..Default::default()
        });
        let url = format!("ws://{}/room", server.addr());
        let mut before_idle = connect_room(url.as_str());
        assert!(before_idle.read().expect("join before idle").is_text());
        before_idle.close(None).expect("close before idle");
        assert!(before_idle.read().expect("close reply").is_close());
        let report = server.wait_until(Duration::from_secs(2), |r| r.active_rooms == 0);
        assert_eq!(report.active_rooms, 0, "{report:?}");

        assert!(connect(url.as_str()).is_err(), "expired room must reject upgrade");
        let created = http_request(server.addr(), "POST", "/rooms/default");
        assert!(created.starts_with("HTTP/1.1 201"), "{created}");
        let mut client = connect_room(url.as_str());
        assert!(client.read().expect("join after idle create").is_text());
        let _ = client.close(None);
        let _ = server.stop();
    }

    #[test]
    fn shutdown_during_broadcast_closes_clients_without_hanging() {
        let _guard = test_guard();
        let server = crate::RoomServer::start();
        let url = format!("ws://{}/room", server.addr());
        let mut a = connect_room(url.as_str());
        let mut b = connect_room(url.as_str());
        assert!(a.read().expect("a join").is_text());
        assert!(b.read().expect("b join").is_text());

        a.send(Message::Text("before-shutdown".into()))
            .expect("send before shutdown");
        assert_eq!(
            b.read().expect("broadcast before shutdown").into_text().expect("text"),
            "room:before-shutdown"
        );

        let report = server.shutdown_room();
        assert_eq!(report.shutdown_close_requested, 2, "{report:?}");
        assert_eq!(
            report.shutdown_close_ok + report.shutdown_close_failed,
            2,
            "{report:?}"
        );
        assert!(connect(url.as_str()).is_err(), "post-shutdown connect must fail");
        let _ = a.close(None);
        let _ = b.close(None);
        let _ = server.stop();
    }

    #[test]
    fn reconnect_refill_loop_does_not_leak_live_members() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            member_capacity: 1,
            ..Default::default()
        });
        let url = format!("ws://{}/room", server.addr());
        for index in 0..25 {
            let mut client = connect_room(url.as_str());
            assert!(client.read().expect("join").is_text(), "index {index}");
            let report = server.wait_until(Duration::from_secs(2), |r| r.live_members == 1);
            assert_eq!(report.live_members, 1, "{report:?}");
            let _ = client.close(None);
            assert!(client.read().expect("close reply").is_close(), "index {index}");
            let report = server.wait_until(Duration::from_secs(2), |r| r.live_members == 0);
            assert_eq!(report.live_members, 0, "{report:?}");
        }
        let report = server.report();
        assert_eq!(report.joined, 25, "{report:?}");
        assert_eq!(report.live_members, 0, "{report:?}");
        let _ = server.stop();
    }

    #[test]
    fn http_routes_coexist_with_websocket_activity() {
        let _guard = test_guard();
        let server = crate::RoomServer::start();
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
        let _ = server.stop();
    }

    #[test]
    fn real_tungstenite_client_works_over_wss() {
        let _guard = test_guard();
        let tls = TlsRoomServer::start();
        let tcp = TcpStream::connect_timeout(&tls.addr, Duration::from_secs(2)).expect("tcp");
        tcp.set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set tls read timeout");
        tcp.set_write_timeout(Some(Duration::from_secs(2)))
            .expect("set tls write timeout");
        let mut stream = rustls_client(tcp, tls.cert_der.clone());
        let request = format!("wss://localhost:{}/room", tls.addr.port());
        let (mut ws, _) = client(request.as_str(), &mut stream).expect("wss connect");
        assert!(ws.read().expect("join").is_text());
        ws.send(Message::Text("tls".into())).expect("send tls");
        let report = tls.wait_until(Duration::from_secs(2), |r| r.joined == 1);
        assert_eq!(report.live_members, 1, "{report:?}");
        let _ = ws.close(None);
        let _ = tls.stop();
    }

    #[test]
    fn many_client_connect_send_shutdown_smoke_stays_bounded() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            member_capacity: 8,
            ..Default::default()
        });
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

        let report = server.shutdown_room();
        assert_eq!(report.shutdown_close_requested, 8, "{report:?}");
        assert_eq!(
            report.shutdown_close_ok + report.shutdown_close_failed,
            8,
            "{report:?}"
        );
        for mut client in clients {
            let _ = client.close(None);
        }
        let _ = server.stop();
    }

    #[test]
    fn ci_short_load_churn_reports_high_water_and_shutdown() {
        let _guard = test_guard();
        let server = crate::RoomServer::start_with(crate::RoomServerConfig {
            member_capacity: 6,
            ..Default::default()
        });
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

        let report = server.shutdown_room();
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
        let after = server.stop();
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

    struct TlsRoomServer {
        addr: std::net::SocketAddr,
        cert_der: Vec<u8>,
        runtime: ThreadedRuntime<crate::DemoShard, DefaultThreadedMailboxFactory>,
        listener: tina::Address<
            HttpsListenerMsg,
            Result<tina_http::HttpsReady, tina_http::HttpsStartupError>,
        >,
        report: std::sync::Arc<crate::SharedReport>,
    }

    impl TlsRoomServer {
        fn start() -> Self {
            let generated = generate_identity();
            let report = std::sync::Arc::new(crate::SharedReport::default());
            report.update(|r| {
                r.active_rooms = 1;
                r.room_capacity = 1;
                r.member_capacity = 3;
                r.room_high_water = 1;
            });
            let limits = WebSocketLimits::default();
            let runtime = ThreadedRuntime::with_config(
                crate::DemoShard,
                DefaultThreadedMailboxFactory,
                ThreadedRuntimeConfig {
                    command_capacity: 64,
                    idle_wait: Duration::from_millis(1),
                    ..Default::default()
                },
            );
            let room = runtime
                .register_with_capacity::<crate::Room, Infallible>(
                    crate::Room {
                        members: std::collections::BTreeMap::new(),
                        member_capacity: 3,
                        idle_room_expiry: Duration::from_secs(60),
                        idle_generation: 0,
                        report: report.clone(),
                        first_closed: None,
                        stale_probe: None,
                        pending_stale_probe: None,
                        shutting_down: false,
                        deleting: false,
                    },
                    16,
                )
                .expect("register room");
            let gateway = runtime
                .register_with_capacity::<crate::Gateway, tina_http::WebSocketSessionMsg>(
                    crate::Gateway {
                        room,
                        limits,
                        admission: crate::AdmissionPolicy::default(),
                        report: report.clone(),
                        room_active: true,
                        idle_room_expiry: Duration::from_secs(60),
                        idle_generation: 0,
                    },
                    16,
                )
                .expect("register gateway");
            let listener_isolate = HttpsListener::<crate::DemoShard>::new(
                "127.0.0.1:0".parse().unwrap(),
                gateway,
                HttpsServerConfig::dev(generated.identity),
            );
            let listener = runtime
                .register_with_capacity::<HttpsListener<crate::DemoShard>, Infallible>(
                    listener_isolate,
                    8,
                )
                .expect("register https listener");
            let ready = runtime
                .call_blocking(listener, HttpsListenerMsg::Start, Duration::from_secs(5))
                .expect("start https");
            let ready = match ready {
                CallOutcome::Replied(Ok(ready)) => ready,
                other => panic!("https listener did not start: {other:?}"),
            };
            Self {
                addr: ready.local_addr,
                cert_der: generated.cert_der,
                runtime,
                listener,
                report,
            }
        }

        fn wait_until(
            &self,
            timeout: Duration,
            f: impl Fn(&crate::RoomReport) -> bool,
        ) -> crate::RoomReport {
            self.report.wait_until(timeout, f)
        }

        fn stop(self) -> crate::RoomReport {
            let report = self.report.snapshot();
            let _ = self.runtime.try_send(self.listener, HttpsListenerMsg::Stop);
            let _ = self.runtime.shutdown();
            report
        }
    }

    struct GeneratedIdentity {
        identity: TlsServerIdentity,
        cert_der: Vec<u8>,
    }

    fn generate_identity() -> GeneratedIdentity {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("rcgen self-sign");
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.signing_key.serialize_der();
        let identity = TlsServerIdentity::from_der(vec![cert_der.clone()], key_der);
        GeneratedIdentity { identity, cert_der }
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
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
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
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
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
