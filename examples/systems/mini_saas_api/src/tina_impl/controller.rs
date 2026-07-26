use std::collections::HashMap;
use std::time::Duration;

use http::StatusCode;
use tina::pool::{
    AcquireOutcome, PoolLease, PoolPressureReport, ReleaseDisposition, ReleaseOutcome,
};
use tina::prelude::*;
use tina::{CallContext, RequestContext, reply_to};
use tina_http::{
    BodyMetrics, BodyPressureReport, HttpRequest, HttpRequestBody, HttpResponse, KeepaliveConnAddr,
    KeepaliveConnectionMsg, KeepaliveOutcome,
};
use tina_runtime::lifecycle::{Lifecycle, Readiness, ReadinessReason};
use tina_runtime::pool::{WorkerPoolMsg, WorkerPoolReply};
use tina_runtime::{
    CallOutcome, DrainStage, DrainState, RequestScope, RequestScopeId, RequestScopeSet, call,
    call_cancelable, sleep,
};
use tina_sqlite_bridge::{
    SqliteAddress, SqliteError, SqliteMetricsHandle, SqlitePressureReport, SqliteRequest,
    SqliteResponse, SqliteResult, SqliteValue, send_request,
};

use super::{REQUEST_TIMEOUT, ScopeSetMetrics};

type PoolAddr = Address<WorkerPoolMsg<KeepaliveConnAddr>, WorkerPoolReply<KeepaliveConnAddr>>;

#[derive(Default)]
pub(crate) struct NotifySink {
    accepted: u64,
}

pub(crate) enum NotifyRequest {
    Http(HttpRequest),
}

impl From<HttpRequest> for NotifyRequest {
    fn from(request: HttpRequest) -> Self {
        Self::Http(request)
    }
}

pub(crate) enum NotifyEvent {
    Delayed(RequestContext<HttpResponse>),
}

#[tina_runtime::isolate(event = NotifyEvent, request = NotifyRequest, reply = HttpResponse)]
impl NotifySink {
    fn handle_event(
        &mut self,
        msg: NotifyEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            NotifyEvent::Delayed(req) => {
                self.accepted += 1;
                reply_to(req, text(StatusCode::OK, "accepted\n"))
            }
        }
    }

    fn handle_request(
        &mut self,
        msg: NotifyRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match msg {
            NotifyRequest::Http(request) => {
                if request.method != http::Method::POST || request.path != "/notify" {
                    return call.reply(text(StatusCode::NOT_FOUND, "missing\n"));
                }
                if body_text(&request).contains("fail") {
                    return call
                        .reply(text(StatusCode::INTERNAL_SERVER_ERROR, "upstream_failed\n"));
                }
                if body_text(&request).contains("close") {
                    self.accepted += 1;
                    let mut response = text(StatusCode::OK, "accepted\n");
                    response.headers.insert(
                        http::header::CONNECTION,
                        http::HeaderValue::from_static("close"),
                    );
                    return call.reply(response);
                }
                if body_text(&request).contains("slow") {
                    return call
                        .defer(sleep(Duration::from_millis(250)))
                        .reply_service_event(|req, _| NotifyEvent::Delayed(req));
                }
                self.accepted += 1;
                call.reply(text(StatusCode::OK, "accepted\n"))
            }
        }
    }
}

pub(crate) struct Controller {
    db: SqliteAddress,
    db_metrics: SqliteMetricsHandle,
    outbound_pool: PoolAddr,
    body_metrics: BodyMetrics,
    /// Public body cap, read from the budget manifest at startup. The
    /// listener enforces the same cap; this is the handler-side check
    /// for buffered bodies that reach the isolate.
    body_cap: usize,
    /// Mailbox cap the controller was registered with, read from the
    /// manifest. Surfaced in `/debug/capacity` so the reported value
    /// comes from the manifest object, not a separate const.
    controller_mailbox: usize,
    next_id: i64,
    /// Public-ingress admission state. The controller drives `Open` →
    /// `Draining` on `CloseIngress`; per-request completion lives in the
    /// listener/runtime, so the helper is used purely for the typed stage
    /// label surfaced in `/debug/capacity`. The host owns terminal proof, so
    /// `drain.finish()` is never called from inside the controller.
    drain: DrainState,
    live_items: HashMap<i64, String>,
    /// One request scope per in-flight `POST /items/{id}/notify`. Capacity
    /// and per-request child cap are installed from the budget manifest.
    notify_scopes: RequestScopeSet<u64>,
    /// Per-request child cap read from the manifest; used to size each
    /// notify request's scope.
    scope_child_cap: usize,
    /// Shared live counters for the scope set, joined into the budget
    /// report at shutdown.
    scope_metrics: ScopeSetMetrics,
    /// Monotonic key for scope-set entries.
    next_scope_id: u64,
    /// Direct notify/outbound facts for soak proof. Pressure counters can be
    /// zero in a healthy serial run; these say the pool lane actually ran.
    notify_attempted: u64,
    outbound_acquired: u64,
    outbound_released: u64,
    outbound_retired: u64,
}

impl Controller {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        db: SqliteAddress,
        db_metrics: SqliteMetricsHandle,
        outbound_pool: PoolAddr,
        body_metrics: BodyMetrics,
        body_cap: usize,
        controller_mailbox: usize,
        scope_set_capacity: usize,
        scope_child_cap: usize,
        scope_metrics: ScopeSetMetrics,
    ) -> Self {
        Self {
            db,
            db_metrics,
            outbound_pool,
            body_metrics,
            body_cap,
            controller_mailbox,
            next_id: 1,
            drain: DrainState::new(),
            live_items: HashMap::new(),
            notify_scopes: RequestScopeSet::with_capacity(scope_set_capacity),
            scope_child_cap,
            scope_metrics,
            next_scope_id: 1,
            notify_attempted: 0,
            outbound_acquired: 0,
            outbound_released: 0,
            outbound_retired: 0,
        }
    }

    /// Retire a notify scope on a terminal branch: drop the set entry and
    /// refresh the live counters. Idempotent; a missing key is fine
    /// (already retired). Every retire point reaches here with no child
    /// rail still pending — the outbound child is registered only after a
    /// lease is acquired and has settled by the time the release returns;
    /// the not-found / acquire-error branches retire before any child is
    /// registered. So dropping the scope cannot strand an open wait.
    fn retire_scope(&mut self, scope_id: u64) {
        if self.notify_scopes.remove(&scope_id).is_ok() {
            self.scope_metrics.observe_in_use(self.notify_scopes.len());
        }
    }

    /// Owner-stop sweep: drain every in-flight request scope and cancel its
    /// still-pending child rails (`OwnerStopped`). Replies with the typed
    /// counts so the host can prove unreleased capacity is zero. A scope
    /// whose outbound child is still parked has its wait closed here; the
    /// late upstream completion stays a visible rejected trace fact.
    fn drain_scopes(&mut self, call: CallContext<'_, Self>) -> Effect<Self> {
        let drained: Vec<(u64, RequestScope)> = self.notify_scopes.drain().collect();
        let scopes_cancelled = drained.len();
        let mut children_cancelled = 0usize;
        let mut effects: Vec<Effect<Self>> = Vec::new();
        for (_key, scope) in drained {
            let (report, cancel_effects) = scope.cancel_into_effects::<Self, _, _>(
                tina_runtime::ScopeCancelCause::OwnerStopped,
                ControllerMsg::ScopeDrained,
            );
            children_cancelled += report.cancelled_count();
            effects.extend(cancel_effects);
        }
        self.scope_metrics.observe_in_use(self.notify_scopes.len());
        let unreleased = self.notify_scopes.len();
        let body = format!(
            "scopes_drained scopes_cancelled={scopes_cancelled} \
             children_cancelled={children_cancelled} unreleased={unreleased}\n"
        );
        effects.push(call.reply(text(StatusCode::OK, body)));
        batch(effects)
    }
}

pub(crate) enum ControllerMsg {
    Http(HttpRequest),
    CloseIngress,
    Notify(NotifyFlow),
    ReadyDb(
        RequestContext<HttpResponse>,
        bool,
        CallOutcome<SqliteResult>,
    ),
    ReadyPool(
        RequestContext<HttpResponse>,
        bool,
        CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
    ),
    Created(
        RequestContext<HttpResponse>,
        i64,
        String,
        CallOutcome<SqliteResult>,
    ),
    Loaded(RequestContext<HttpResponse>, i64, CallOutcome<SqliteResult>),
    CapacityPool(
        RequestContext<HttpResponse>,
        CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
    ),
    /// Owner-stop sweep: drain the request-scope set, cancel every
    /// still-pending child rail, and reply with the typed counts.
    DrainScopes,
    /// One scope child's cancel ack from a drain sweep. The synchronous
    /// [`tina_runtime::ScopeCancelReport`] from the sweep is the
    /// authoritative count; this async per-rail ack is trace-only, so its
    /// payload is intentionally unread here.
    #[allow(dead_code)]
    ScopeDrained(RequestScopeId, &'static str, CancelOutcome),
}

impl From<HttpRequest> for ControllerMsg {
    fn from(request: HttpRequest) -> Self {
        Self::Http(request)
    }
}

tina::flow! {
    pub(crate) flow NotifyFlow for Controller {
        reply HttpResponse;

        step Loaded(scope_id: u64, id: i64, slow: bool) -> SqliteResult {
            match item_from_rows(id, outcome) {
                Ok(Some(name)) => {
                    call(self.outbound_pool, WorkerPoolMsg::Acquire, REQUEST_TIMEOUT)
                        .then_with_request(req, move |req, outcome| {
                            ControllerMsg::Notify(NotifyFlow::Acquired(
                                req, scope_id, id, name, slow, outcome,
                            ))
                        })
                }
                Ok(None) => {
                    self.retire_scope(scope_id);
                    reply_to(req, text(StatusCode::NOT_FOUND, "not_found\n"))
                }
                Err(response) => {
                    self.retire_scope(scope_id);
                    reply_to(req, *response)
                }
            }
        }

        step Acquired(scope_id: u64, id: i64, name: String, slow: bool)
            -> WorkerPoolReply<KeepaliveConnAddr>
        {
            match outcome {
                CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(
                    lease,
                ))) => {
                    self.outbound_acquired += 1;
                    let body = if slow {
                        format!("id={id}&name={name}&slow=true")
                    } else {
                        format!("id={id}&name={name}")
                    };
                    let request = HttpRequest::post("/notify").text_body(body).build();
                    // The outbound request call is the request's cancelable
                    // child: register it into the scope so a scope cancel
                    // closes the parked wait.
                    let (effect, handle) = call_cancelable(
                        *lease.handle(),
                        KeepaliveConnectionMsg::request(request, REQUEST_TIMEOUT),
                        REQUEST_TIMEOUT + Duration::from_secs(1),
                    )
                    .then(move |outcome| {
                        ControllerMsg::Notify(NotifyFlow::Sent(req, scope_id, lease, outcome))
                    });
                    if let Some(scope) = self.notify_scopes.get(&scope_id) {
                        scope.register("outbound_request", handle).expect(
                            "fresh notify scope has room for the single outbound child rail",
                        );
                    }
                    effect
                }
                other => {
                    self.retire_scope(scope_id);
                    reply_to(req, pool_acquire_error_response(other))
                }
            }
        }

        step Sent(scope_id: u64, lease: PoolLease<KeepaliveConnAddr>) -> KeepaliveOutcome {
            let (ok, disposition) = match &outcome {
                CallOutcome::Replied(KeepaliveOutcome::Request {
                    result: Ok(response),
                    ..
                }) => (response.status.is_success(), ReleaseDisposition::Reuse),
                _ => (false, ReleaseDisposition::Retire),
            };
            call(
                self.outbound_pool,
                WorkerPoolMsg::Release { lease, disposition },
                REQUEST_TIMEOUT,
            )
            .then_with_request(req, move |req, release| {
                ControllerMsg::Notify(NotifyFlow::Released(req, scope_id, ok, release))
            })
        }

        step Released(scope_id: u64, ok: bool) -> WorkerPoolReply<KeepaliveConnAddr> {
            self.retire_scope(scope_id);
            match &outcome {
                CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) => {
                    self.outbound_released += 1;
                }
                CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Retired)) => {
                    self.outbound_retired += 1;
                }
                _ => {}
            }
            match outcome {
                CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released))
                    if ok =>
                {
                    reply_to(req, text(StatusCode::OK, "notified\n"))
                }
                CallOutcome::Replied(WorkerPoolReply::Release(_)) if ok => reply_to(
                    req,
                    text(StatusCode::SERVICE_UNAVAILABLE, "outbound_release\n"),
                ),
                _ => reply_to(req, text(StatusCode::BAD_GATEWAY, "notify_failed\n")),
            }
        }
    }
}

#[tina_runtime::isolate(message = ControllerMsg, reply = HttpResponse)]
impl Controller {
    fn handle(
        &mut self,
        msg: ControllerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ControllerMsg::Http(_) => noop(),
            ControllerMsg::CloseIngress => {
                self.drain.begin();
                noop()
            }
            ControllerMsg::Notify(flow) => self.handle_notify_flow(flow),
            ControllerMsg::ReadyDb(req, ingress_stopped, outcome) => match outcome {
                CallOutcome::Replied(Ok(_)) => call(
                    self.outbound_pool,
                    WorkerPoolMsg::PressureReport,
                    REQUEST_TIMEOUT,
                )
                .then_with_request(req, move |req, outcome| {
                    ControllerMsg::ReadyPool(req, ingress_stopped, outcome)
                }),
                other => reply_to(
                    req,
                    readiness_response(&build_readiness(ingress_stopped, Some(db_reason(&other)))),
                ),
            },
            ControllerMsg::ReadyPool(req, ingress_stopped, outcome) => {
                let dep = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Pressure(report))
                        if report.available > 0 =>
                    {
                        None
                    }
                    CallOutcome::Replied(WorkerPoolReply::Pressure(_)) => {
                        Some(ReadinessReason::DependencyFull("outbound"))
                    }
                    // Readiness policy: any non-pressure outcome cannot
                    // prove availability and reads as not-ready/closed.
                    CallOutcome::Replied(_)
                    | CallOutcome::Full
                    | CallOutcome::Closed
                    | CallOutcome::Timeout
                    | CallOutcome::Rejected(_) => {
                        Some(ReadinessReason::DependencyClosed("outbound"))
                    }
                };
                reply_to(
                    req,
                    readiness_response(&build_readiness(ingress_stopped, dep)),
                )
            }
            ControllerMsg::Created(req, id, name, outcome) => match outcome {
                CallOutcome::Replied(Ok(SqliteResponse::Executed { .. })) => {
                    self.live_items.insert(id, name);
                    reply_to(req, text(StatusCode::CREATED, format!("id={id}\n")))
                }
                CallOutcome::Replied(Err(SqliteError::Constraint(_))) => {
                    reply_to(req, text(StatusCode::CONFLICT, "db_constraint\n"))
                }
                other => reply_to(req, db_error_response(other)),
            },
            ControllerMsg::Loaded(req, id, outcome) => reply_to(req, item_response(id, outcome)),
            ControllerMsg::CapacityPool(req, outcome) => {
                let body = self.body_metrics.snapshot();
                let db = self.db_metrics.pressure_report();
                let outbound = match outcome {
                    CallOutcome::Replied(WorkerPoolReply::Pressure(report)) => report,
                    // Capacity policy: a non-pressure outcome reports the
                    // default pressure rather than a synthesized success.
                    CallOutcome::Replied(_)
                    | CallOutcome::Full
                    | CallOutcome::Closed
                    | CallOutcome::Timeout
                    | CallOutcome::Rejected(_) => PoolPressureReport::default(),
                };
                reply_to(
                    req,
                    text(
                        StatusCode::OK,
                        capacity_body(
                            body,
                            db,
                            outbound,
                            self.drain.stage(),
                            self.body_cap,
                            self.controller_mailbox,
                            self.notify_attempted,
                            self.outbound_acquired,
                            self.outbound_released,
                            self.outbound_retired,
                        ),
                    ),
                )
            }
            // DrainScopes is call-only; a stray send is a no-op. The cancel
            // acks from a drain sweep are observability only.
            ControllerMsg::DrainScopes => noop(),
            ControllerMsg::ScopeDrained(_, _, _) => noop(),
        }
    }

    fn handle_call(&mut self, msg: ControllerMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            ControllerMsg::Http(request) => self.route(request, call),
            ControllerMsg::CloseIngress => {
                self.drain.begin();
                call.reply(text(StatusCode::OK, "ingress_closed\n"))
            }
            ControllerMsg::DrainScopes => self.drain_scopes(call),
            _ => call.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl Controller {
    fn route(&mut self, request: HttpRequest, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        let method = request.method.clone();
        let path = request.path.clone();
        match (method, path.as_str()) {
            (http::Method::GET, "/health") => call_ctx.reply(text(StatusCode::OK, "alive\n")),
            (http::Method::GET, "/ready") => {
                let ingress_stopped = !self.drain.is_open();
                call_ctx
                    .defer(send_request(
                        self.db,
                        SqliteRequest::query_rows("SELECT 1", 1),
                        REQUEST_TIMEOUT,
                    ))
                    .reply(move |req, outcome| {
                        ControllerMsg::ReadyDb(req, ingress_stopped, outcome)
                    })
            }
            (http::Method::GET, "/debug/capacity") => call_ctx
                .defer(call(
                    self.outbound_pool,
                    WorkerPoolMsg::PressureReport,
                    REQUEST_TIMEOUT,
                ))
                .reply(ControllerMsg::CapacityPool),
            _ if !self.drain.is_open() => {
                call_ctx.reply(text(StatusCode::SERVICE_UNAVAILABLE, "ingress_stopped\n"))
            }
            (http::Method::POST, "/items") => self.create_item(request, call_ctx),
            _ if path.starts_with("/items/") => self.item_route(request, call_ctx),
            _ => call_ctx.reply(text(StatusCode::NOT_FOUND, "not_found\n")),
        }
    }

    fn create_item(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        let Some(body) = request.body.as_buffered() else {
            return call.reply(text(
                StatusCode::BAD_REQUEST,
                "streaming_body_unsupported\n",
            ));
        };
        if body.len() > self.body_cap {
            return call.reply(text(StatusCode::PAYLOAD_TOO_LARGE, "body_full\n"));
        }
        let body = String::from_utf8_lossy(body);
        let Some(name) = body.strip_prefix("name=").filter(|s| !s.is_empty()) else {
            return call.reply(text(StatusCode::BAD_REQUEST, "bad_request\n"));
        };
        let id = self.next_id;
        self.next_id += 1;
        let name = name.to_owned();
        call.defer(send_request(
            self.db,
            SqliteRequest::execute("INSERT INTO items (id, name) VALUES (?, ?)").params(vec![
                SqliteValue::Integer(id),
                SqliteValue::Text(name.clone()),
            ]),
            REQUEST_TIMEOUT,
        ))
        .reply(move |req, outcome| ControllerMsg::Created(req, id, name, outcome))
    }

    fn item_route(&mut self, request: HttpRequest, call: CallContext<'_, Self>) -> Effect<Self> {
        let rest = request.path.trim_start_matches("/items/");
        let (id_text, notify) = match rest.strip_suffix("/notify") {
            Some(id) => (id, true),
            None => (rest, false),
        };
        let Ok(id) = id_text.parse::<i64>() else {
            return call.reply(text(StatusCode::BAD_REQUEST, "bad_id\n"));
        };
        match (&request.method, notify) {
            (&http::Method::GET, false) => call
                .defer(send_request(
                    self.db,
                    SqliteRequest::query_rows("SELECT name FROM items WHERE id = ?", 1)
                        .params(vec![SqliteValue::Integer(id)]),
                    REQUEST_TIMEOUT,
                ))
                .reply(move |req, outcome| ControllerMsg::Loaded(req, id, outcome)),
            (&http::Method::POST, true) => {
                let slow = body_text(&request).contains("slow");
                // One scope per notify request, sized from the manifest.
                // Admit it before dispatching any child work; a full set
                // sheds with a typed answer and dispatches nothing.
                let scope_id = self.next_scope_id;
                self.next_scope_id += 1;
                let scope =
                    RequestScope::with_child_cap(RequestScopeId::alloc(), self.scope_child_cap);
                if self.notify_scopes.try_insert(scope_id, scope).is_err() {
                    self.scope_metrics.on_full();
                    return call.reply(text(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "request_scopes_full\n",
                    ));
                }
                self.notify_attempted += 1;
                self.scope_metrics.observe_in_use(self.notify_scopes.len());
                call.defer(send_request(
                    self.db,
                    SqliteRequest::query_rows("SELECT name FROM items WHERE id = ?", 1)
                        .params(vec![SqliteValue::Integer(id)]),
                    REQUEST_TIMEOUT,
                ))
                .reply(move |req, outcome| {
                    ControllerMsg::Notify(NotifyFlow::Loaded(req, scope_id, id, slow, outcome))
                })
            }
            _ => call.reply(text(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed\n")),
        }
    }
}

fn item_response(id: i64, outcome: CallOutcome<SqliteResult>) -> HttpResponse {
    match item_from_rows(id, outcome) {
        Ok(Some(name)) => text(StatusCode::OK, format!("id={id} name={name}\n")),
        Ok(None) => text(StatusCode::NOT_FOUND, "not_found\n"),
        Err(response) => *response,
    }
}

fn item_from_rows(
    id: i64,
    outcome: CallOutcome<SqliteResult>,
) -> Result<Option<String>, Box<HttpResponse>> {
    match outcome {
        CallOutcome::Replied(Ok(SqliteResponse::Rows { rows, .. })) => {
            let Some(row) = rows.first() else {
                return Ok(None);
            };
            let Some(name) = row.first().and_then(SqliteValue::as_text) else {
                return Err(Box::new(text(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "decode_error\n",
                )));
            };
            let _ = id;
            Ok(Some(name.to_owned()))
        }
        other => Err(Box::new(db_error_response(other))),
    }
}

fn db_error_response(outcome: CallOutcome<SqliteResult>) -> HttpResponse {
    match outcome {
        CallOutcome::Replied(Err(SqliteError::Full)) | CallOutcome::Full => {
            text(StatusCode::SERVICE_UNAVAILABLE, "db_full\n")
        }
        CallOutcome::Replied(Err(SqliteError::Closed)) | CallOutcome::Closed => {
            text(StatusCode::SERVICE_UNAVAILABLE, "db_closed\n")
        }
        CallOutcome::Replied(Err(SqliteError::Timeout)) | CallOutcome::Timeout => {
            text(StatusCode::GATEWAY_TIMEOUT, "db_timeout\n")
        }
        CallOutcome::Replied(Err(SqliteError::Constraint(_))) => {
            text(StatusCode::CONFLICT, "db_constraint\n")
        }
        CallOutcome::Replied(Err(_)) | CallOutcome::Rejected(_) => {
            text(StatusCode::INTERNAL_SERVER_ERROR, "db_error\n")
        }
        CallOutcome::Replied(Ok(_)) => text(StatusCode::INTERNAL_SERVER_ERROR, "db_shape\n"),
    }
}

fn db_reason(outcome: &CallOutcome<SqliteResult>) -> ReadinessReason {
    match outcome {
        CallOutcome::Replied(Err(SqliteError::Closed)) | CallOutcome::Closed => {
            ReadinessReason::DependencyClosed("db")
        }
        CallOutcome::Replied(Err(SqliteError::Full)) | CallOutcome::Full => {
            ReadinessReason::DependencyFull("db")
        }
        CallOutcome::Replied(Err(SqliteError::Timeout)) | CallOutcome::Timeout => {
            ReadinessReason::DependencyTimeout("db")
        }
        // Readiness policy: any reply not named above (ok shape or an
        // unlisted SqliteError) or a rejected call is a dependency error,
        // distinct from closed/full/timeout.
        CallOutcome::Replied(_) | CallOutcome::Rejected(_) => {
            ReadinessReason::DependencyError("db")
        }
    }
}

fn pool_acquire_error_response(
    outcome: CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
) -> HttpResponse {
    match outcome {
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Full))
        | CallOutcome::Full => text(StatusCode::SERVICE_UNAVAILABLE, "outbound_full\n"),
        CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Closed))
        | CallOutcome::Closed => text(StatusCode::SERVICE_UNAVAILABLE, "outbound_closed\n"),
        CallOutcome::Timeout => text(StatusCode::GATEWAY_TIMEOUT, "outbound_timeout\n"),
        // Acquire policy: an unexpected reply or a rejected call maps to
        // the same 503 body, named separately from full/closed/timeout.
        CallOutcome::Replied(_) | CallOutcome::Rejected(_) => {
            text(StatusCode::SERVICE_UNAVAILABLE, "outbound_unavailable\n")
        }
    }
}

fn readiness_response(readiness: &Readiness) -> HttpResponse {
    let status = if readiness.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    text(status, readiness.legacy_body())
}

// Build the typed `Readiness` for the controller's `/ready` route. The
// state picks Draining when ingress is closed, NotReady when a dependency
// is the only issue, and Ready when both are clean.
fn build_readiness(ingress_stopped: bool, dep: Option<ReadinessReason>) -> Readiness {
    let mut reasons = Vec::with_capacity(2);
    if ingress_stopped {
        reasons.push(ReadinessReason::IngressStopped);
    }
    if let Some(reason) = dep {
        reasons.push(reason);
    }
    if reasons.is_empty() {
        Readiness::ready()
    } else {
        let state = if ingress_stopped {
            Lifecycle::Draining
        } else {
            Lifecycle::NotReady
        };
        Readiness::not_ready(state, reasons)
    }
}

// One flat `/debug/capacity` line joins every live surface; the args are the
// snapshots, kept positional so the format string reads as the wire format.
#[allow(clippy::too_many_arguments)]
fn capacity_body(
    body: BodyPressureReport,
    db: SqlitePressureReport,
    outbound: PoolPressureReport,
    drain_stage: DrainStage,
    body_cap: usize,
    controller_mailbox: usize,
    notify_attempted: u64,
    outbound_acquired: u64,
    outbound_released: u64,
    outbound_retired: u64,
) -> String {
    let stage = drain_stage_label(drain_stage);
    format!(
        "http.body_cap={body_cap} http.request_body_current={} \
         http.request_body_high_water={} http.response_body_current={} \
         http.response_body_high_water={} http.body_full={} http.body_timeout={} \
         http.body_io_error={} \
         controller.mailbox={controller_mailbox} drain.stage={stage} \
         notify.attempted={notify_attempted} outbound.acquired={outbound_acquired} \
         outbound.released={outbound_released} outbound.retired={outbound_retired} \
         db.capacity={} db.waiters={} db.max_waiters={} db.in_flight={} db.high_water={} \
         db.full={} db.closed={} db.timeout={} outbound.capacity={} outbound.waiters={} \
         outbound.max_waiters={} outbound.in_flight={} outbound.high_water_waiters={} \
         outbound.full={} outbound.closed={} outbound.closed_count={} outbound.cancel={}\n",
        body.request_body_current,
        body.request_body_high_water,
        body.response_body_current,
        body.response_body_high_water,
        body.body_full_count,
        body.body_timeout_count,
        body.body_io_error_count,
        db.capacity,
        db.waiters,
        db.max_waiters,
        db.leased,
        db.high_water,
        db.full_count,
        db.closed_count,
        db.timeout_count,
        outbound.capacity,
        outbound.waiters,
        outbound.max_waiters,
        outbound.leased,
        outbound.high_water_waiters,
        outbound.full_count,
        outbound.closed,
        outbound.closed_count,
        outbound.cancel_count,
    )
}

// `Stopped` is unreachable in this specimen: the host owns the terminal
// report and tears down the controller before `drain.finish()` would run.
// The arm exists so the label match stays exhaustive against `DrainStage`.
fn drain_stage_label(stage: DrainStage) -> &'static str {
    match stage {
        DrainStage::Open => "open",
        DrainStage::Draining => "draining",
        DrainStage::Stopped => "stopped",
    }
}

fn body_text(request: &HttpRequest) -> String {
    match &request.body {
        HttpRequestBody::Buffered(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        HttpRequestBody::Stream(_) | HttpRequestBody::Http2Stream(_) => String::new(),
    }
}

fn text(status: StatusCode, body: impl Into<String>) -> HttpResponse {
    let mut response = HttpResponse::with_body(status, body.into().into_bytes());
    response.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/plain"),
    );
    response
}
