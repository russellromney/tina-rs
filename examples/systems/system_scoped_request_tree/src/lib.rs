//! One HTTP request, one request tree.
//!
//! The small specimen for request-scoped cancellation. It keeps the
//! surface tiny on purpose: one route (`POST /upload`), one streaming
//! request body, one tombstoned request-deadline timer, one cancelable
//! child rail, and one [`ScopedRequestReport`] per torn-down request.
//!
//! The story it proves:
//!
//! - A request owns a [`RequestScope`] held in a bounded
//!   [`RequestScopeSet`]. The cancelable enrich child registers into the
//!   scope, so a scope cancel closes its wait.
//! - The request deadline is a [`ScopedTimer`]: plain `sleep` cannot be
//!   `CallHandle`-cancelled, so cancelling the request *tombstones* the
//!   timer ticket. When the physical sleep fires later it is observed as
//!   [`ScopedTimerFire::IgnoredLate`] — counted, never run.
//! - When the client disconnects mid-body, the streaming pull reports a
//!   short body. The controller cancels the scope (`ClientDisconnect`),
//!   the enrich child's wait closes, the timer is tombstoned, and the
//!   request's slot is reclaimed.
//!
//! No fake cancellation: external work that already started is not
//! "un-started"; the report names what was cancelled, what had settled,
//! and how much capacity came back.

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::StatusCode;
use tina::capacity::{CapacityMode, CapacitySurfaceReport};
use tina::prelude::*;
use tina::{RequestContext, reply_to};
use tina_http::{
    HttpConnectionMsg, HttpLimits, HttpListener, HttpListenerMsg, HttpRequest, HttpRequestBody,
    HttpResponse, RequestChunkReply,
};
use tina_runtime::{
    CallOutcome, DefaultThreadedMailboxFactory, EventId, RequestScope, RequestScopeId,
    RequestScopeSet, RuntimeEvent, ScopeCancelCause, ScopedRequestReport, ScopedTimer,
    ScopedTimerFire, ScopedTimerId, ScopedTimerSet, ThreadedRuntime, ThreadedRuntimeConfig, call,
    call_cancelable, sleep,
};
use tina_sim::dst::{
    LiveReplayCapture, LiveReplayFact, LiveReplayReport, ReplayCase as DstReplayCase, ReplayConfig,
    ReplayReport, TraceProjectionError, check_captured_replay,
};

/// Per-request deadline. Short so the tombstoned timer fires inside the
/// test window; a complete request settles in microseconds on loopback,
/// so the margin to the deadline is large.
const REQUEST_DEADLINE: Duration = Duration::from_millis(100);
const CHILD_TIMEOUT: Duration = Duration::from_secs(5);
const CHUNK_SIZE: usize = 64;
/// Concurrent request scopes the service will admit. This specimen serves
/// **one upload at a time**: the shared `Enrich` worker holds a single
/// deferred slot, so admitting a second concurrent upload would clobber
/// the first's slot. A second concurrent upload sheds with `503
/// scope_set_full` rather than corrupting the first. Concurrency is the
/// larger service's job (`mini_saas_api`), not this small proof.
const SCOPE_SET_CAPACITY: usize = 1;
/// Per-request child rails. One enrich child is the whole tree here.
const SCOPE_CHILD_CAP: usize = 1;
/// Timers tracked at once. A tombstoned timer keeps its slot until its
/// physical sleep fires (`IgnoredLate`), so the cap must cover the live
/// timer **plus** every recently-completed request's tombstone still
/// inside its deadline window. This specimen drives a short burst of
/// requests faster than the deadline, so several tombstones coexist;
/// size the cap with headroom for that, not just for one live timer.
const TIMER_CAP: usize = 8;

type RequestId = u64;

/// Typed outcome of one [`run`] of the specimen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedTreeReport {
    /// The happy-path request streamed its full body and got `200`.
    pub clean_completion: bool,
    /// Children whose wait was still open when the disconnect cancel ran.
    pub disconnect_cancelled_children: usize,
    /// The disconnect report's cause was `ClientDisconnect`.
    pub disconnect_cause_client: bool,
    /// The disconnect report had no unsupported rails (clean).
    pub disconnect_report_clean: bool,
    /// Tombstoned timers that fired late and were ignored across the run.
    pub timers_ignored_late: u64,
    /// The scope set reclaimed every slot after the disconnect teardown.
    pub scope_capacity_reclaimed: bool,
    /// A request that blew its deadline got a live-timer (`Run`) teardown
    /// and a user-visible `504`.
    pub timeout_replied_504: bool,
    /// Child results that arrived after their scope was cancelled.
    pub late_results: usize,
    /// The enrich rail's async cancel ack came back `Cancelled` (the wait
    /// was really closed, not merely already-settled).
    pub enrich_cancel_ack_cancelled: bool,
    /// One-line summary of the disconnect request's `ScopedRequestReport`.
    pub disconnect_report_line: String,
    /// Sim/replay agreement line for the request-scope-set surface.
    pub replay_fact_line: String,
}

impl ScopedTreeReport {
    /// One grep-friendly status line.
    pub fn summary_line(&self) -> String {
        format!(
            "system=scoped_request_tree clean_completion={} disconnect_cancelled_children={} \
             disconnect_cause_client={} disconnect_report_clean={} timers_ignored_late={} \
             scope_capacity_reclaimed={} timeout_replied_504={} late_results={}",
            self.clean_completion,
            self.disconnect_cancelled_children,
            self.disconnect_cause_client,
            self.disconnect_report_clean,
            self.timers_ignored_late,
            self.scope_capacity_reclaimed,
            self.timeout_replied_504,
            self.late_results,
        )
    }
}

// ---- Observations shared with the host thread ----

#[derive(Debug, Default)]
struct TreeObs {
    clean_completions: usize,
    disconnect_reports: Vec<ScopedRequestReport>,
    timeout_reports: Vec<ScopedRequestReport>,
    timers_ignored_late: u64,
    late_results: usize,
    /// Per-rail cancel acks the scope produced: (label, outcome).
    child_cancel_acks: Vec<(&'static str, CancelOutcome)>,
}

// ---- Enrich worker: one cancelable child rail. ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnrichReply;

#[derive(Debug)]
enum EnrichMsg {
    /// Held until the controller releases it; the worker-return path is
    /// the request's enrich step.
    Work,
    /// Release the held call so the worker replies (happy path).
    Release,
}

#[derive(Default)]
struct Enrich {
    held: Option<DeferredReply<EnrichReply>>,
}

#[tina_runtime::isolate(message = EnrichMsg, reply = EnrichReply)]
impl Enrich {
    fn handle(
        &mut self,
        msg: EnrichMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            EnrichMsg::Work => noop(),
            EnrichMsg::Release => match self.held.take() {
                Some(slot) => reply_to(slot, EnrichReply),
                None => noop(),
            },
        }
    }

    fn handle_call(&mut self, msg: EnrichMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            EnrichMsg::Work => {
                self.held = Some(call.into_request_context().into_deferred());
                noop()
            }
            // Release answers the held `Work` call (the request's enrich
            // result) and acks the release caller. Both replies are real;
            // nothing is faked.
            EnrichMsg::Release => match self.held.take() {
                Some(slot) => batch(vec![reply_to(slot, EnrichReply), call.reply(EnrichReply)]),
                None => call.reply(EnrichReply),
            },
        }
    }
}

// ---- Controller: one route, one request tree per call. ----

struct PendingTree {
    req: Option<RequestContext<HttpResponse>>,
    scope: RequestScope,
    timer: ScopedTimer,
    source: Address<HttpConnectionMsg, RequestChunkReply>,
    expected: usize,
    accumulated: usize,
    body_done: bool,
    enrich_done: bool,
}

/// Split-service request: the only inbound authority this tree ever sees
/// is one HTTP request. `TreeRequest` is local to this crate, so
/// `From<HttpRequest>` on it is an ordinary, orphan-legal impl — unlike
/// `tina::ServiceMessage<TreeEvent, TreeRequest>`, which is foreign on
/// both sides (defined in `tina`, built from `tina_http::HttpRequest`) and
/// could never be `impl From<HttpRequest> for ..` here. `tina_http`
/// supplies that half generically (see `FromHttpRequest`), so
/// `HttpListener<SingleShard, ServiceMessage<TreeEvent, TreeRequest>>`
/// only needs this plain impl below to wire up.
struct TreeRequest(HttpRequest);

impl From<HttpRequest> for TreeRequest {
    fn from(request: HttpRequest) -> Self {
        Self(request)
    }
}

/// Split-service events: every async continuation the tree reports to
/// itself. None of these carry caller authority.
enum TreeEvent {
    Chunk(RequestId, CallOutcome<RequestChunkReply>),
    EnrichDone(RequestId, CallOutcome<EnrichReply>),
    EnrichReleased,
    Deadline(RequestId, ScopedTimerId),
    ChildCancelled(&'static str, CancelOutcome),
}

struct Tree {
    enrich: Address<EnrichMsg, EnrichReply>,
    scopes: RequestScopeSet<RequestId>,
    timers: ScopedTimerSet,
    pending: HashMap<RequestId, PendingTree>,
    next_id: RequestId,
    obs: Arc<Mutex<TreeObs>>,
}

#[tina_runtime::isolate(event = TreeEvent, request = TreeRequest, reply = HttpResponse)]
impl Tree {
    fn handle_event(
        &mut self,
        event: TreeEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            TreeEvent::Chunk(id, outcome) => self.on_chunk(id, outcome),
            TreeEvent::EnrichDone(id, outcome) => self.on_enrich_done(id, outcome),
            TreeEvent::EnrichReleased => noop(),
            TreeEvent::Deadline(id, timer_id) => self.on_deadline(id, timer_id),
            TreeEvent::ChildCancelled(label, outcome) => {
                self.obs
                    .lock()
                    .expect("obs")
                    .child_cancel_acks
                    .push((label, outcome));
                noop()
            }
        }
    }

    fn handle_request(
        &mut self,
        request: TreeRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        self.on_inbound(request.0, call)
    }
}

impl Tree {
    fn on_inbound(
        &mut self,
        request: HttpRequest,
        call_ctx: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        if request.method != http::Method::POST || request.path != "/upload" {
            return call_ctx.reply(text(StatusCode::NOT_FOUND, "not_found\n"));
        }
        let HttpRequestBody::Stream(stream) = request.body else {
            // A buffered body has no streaming rail to scope-cancel; this
            // specimen only demonstrates the streaming tree.
            return call_ctx.reply(text(StatusCode::BAD_REQUEST, "expected_streaming_body\n"));
        };

        let id = self.next_id;
        self.next_id += 1;
        let scope = RequestScope::with_child_cap(RequestScopeId::alloc(), SCOPE_CHILD_CAP);
        if self.scopes.try_insert(id, scope.clone()).is_err() {
            // Scope set full: shed deliberately, do not dispatch child work.
            return call_ctx.reply(text(StatusCode::SERVICE_UNAVAILABLE, "scope_set_full\n"));
        }
        let timer = match self.timers.arm("request_deadline") {
            Ok(timer) => timer,
            Err(_) => {
                let _ = self.scopes.remove(&id);
                return call_ctx.reply(text(StatusCode::SERVICE_UNAVAILABLE, "timer_set_full\n"));
            }
        };

        // Start the cancelable enrich child and register it into the scope.
        let (enrich_effect, handle) = call_cancelable(self.enrich, EnrichMsg::Work, CHILD_TIMEOUT)
            .then_service_event(move |outcome| TreeEvent::EnrichDone(id, outcome));
        if scope.register("enrich", handle).is_err() {
            // Child cap is sized for exactly this rail; a failure is a bug.
            let _ = self.scopes.remove(&id);
            let _ = self.timers.cancel(timer.id());
            return call_ctx.reply(text(
                StatusCode::INTERNAL_SERVER_ERROR,
                "scope_child_admission_failed\n",
            ));
        }

        let timer_id = timer.id();
        let deadline_effect: Effect<Self> =
            sleep(REQUEST_DEADLINE).then_service_event(move |_| TreeEvent::Deadline(id, timer_id));

        let source = stream.source;
        let expected = stream.content_length;
        let pull = call(source, HttpConnectionMsg::body_next(), CHILD_TIMEOUT)
            .then_service_event(move |outcome| TreeEvent::Chunk(id, outcome));

        call_ctx.capture(move |req_ctx| {
            self.pending.insert(
                id,
                PendingTree {
                    req: Some(req_ctx),
                    scope,
                    timer,
                    source,
                    expected,
                    accumulated: 0,
                    body_done: false,
                    enrich_done: false,
                },
            );

            batch(vec![enrich_effect, deadline_effect, pull])
        })
    }

    fn on_chunk(&mut self, id: RequestId, outcome: CallOutcome<RequestChunkReply>) -> Effect<Self> {
        let Some(tree) = self.pending.get_mut(&id) else {
            return noop();
        };
        match outcome {
            CallOutcome::Replied(RequestChunkReply::Chunk(bytes)) => {
                tree.accumulated += bytes.len();
                let source = tree.source;
                call(source, HttpConnectionMsg::body_next(), CHILD_TIMEOUT)
                    .then_service_event(move |outcome| TreeEvent::Chunk(id, outcome))
            }
            CallOutcome::Replied(RequestChunkReply::Eof) => {
                if tree.accumulated < tree.expected {
                    // The wire was short: the client disconnected mid-body.
                    self.teardown(id, ScopeCancelCause::ClientDisconnect, None)
                } else {
                    tree.body_done = true;
                    let enrich = self.enrich;
                    // Release the held enrich child so the request can
                    // complete on its worker-return path. The ack is
                    // ignored; the real completion is the `EnrichDone`
                    // worker-return.
                    call(enrich, EnrichMsg::Release, CHILD_TIMEOUT)
                        .then_service_event(|_| TreeEvent::EnrichReleased)
                }
            }
            CallOutcome::Replied(RequestChunkReply::Error(_)) => {
                // Truncated read: treat as client disconnect.
                self.teardown(id, ScopeCancelCause::ClientDisconnect, None)
            }
            CallOutcome::Replied(RequestChunkReply::WebSocketSend(_))
            | CallOutcome::Replied(RequestChunkReply::WebSocketReport(_)) => {
                self.teardown(id, ScopeCancelCause::User, None)
            }
            CallOutcome::Closed | CallOutcome::Full | CallOutcome::Rejected(_) => {
                self.teardown(id, ScopeCancelCause::ClientDisconnect, None)
            }
            CallOutcome::Timeout => self.teardown(id, ScopeCancelCause::Timeout, None),
        }
    }

    fn on_enrich_done(&mut self, id: RequestId, outcome: CallOutcome<EnrichReply>) -> Effect<Self> {
        let Some(tree) = self.pending.get_mut(&id) else {
            // Scope already cancelled and removed: this is a late result.
            // It is a rejected trace fact, never delivered to the caller.
            self.obs.lock().expect("obs").late_results += 1;
            return noop();
        };
        match outcome {
            CallOutcome::Replied(EnrichReply) => {
                tree.enrich_done = true;
                if tree.body_done {
                    self.finish_ok(id)
                } else {
                    noop()
                }
            }
            _ => self.teardown(id, ScopeCancelCause::User, None),
        }
    }

    fn on_deadline(&mut self, id: RequestId, timer_id: ScopedTimerId) -> Effect<Self> {
        match self.timers.observe_fire(timer_id) {
            ScopedTimerFire::Run { .. } => {
                // The request outlived its deadline.
                self.teardown(
                    id,
                    ScopeCancelCause::Timeout,
                    Some(text(StatusCode::GATEWAY_TIMEOUT, "request_timeout\n")),
                )
            }
            ScopedTimerFire::IgnoredLate { .. } => {
                self.obs.lock().expect("obs").timers_ignored_late = self.timers.ignored_late();
                noop()
            }
            ScopedTimerFire::Unknown => noop(),
        }
    }

    fn finish_ok(&mut self, id: RequestId) -> Effect<Self> {
        let Some(mut tree) = self.pending.remove(&id) else {
            return noop();
        };
        // Tombstone the deadline: the request is done, but the physical
        // sleep may still fire and must be ignored.
        let _ = self.timers.cancel(tree.timer.id());
        let _ = self.scopes.remove(&id);
        self.obs.lock().expect("obs").clean_completions += 1;
        match tree.req.take() {
            Some(req) => reply_to(req, text(StatusCode::OK, "tree_ok\n")),
            None => noop(),
        }
    }

    /// Cancel the request's scope and produce its [`ScopedRequestReport`].
    fn teardown(
        &mut self,
        id: RequestId,
        cause: ScopeCancelCause,
        reply: Option<HttpResponse>,
    ) -> Effect<Self> {
        let Some(mut tree) = self.pending.remove(&id) else {
            return noop();
        };
        // Tombstone the deadline timer (a real sleep may still fire late).
        let _ = self.timers.cancel(tree.timer.id());
        let (cancel_report, cancel_effect) = tree
            .scope
            .cancel_into_service_event_effect::<Self, _, _, _>(
                cause,
                move |_scope, label, outcome| TreeEvent::ChildCancelled(label, outcome),
            );
        // Remove from the set, then snapshot capacity so the report shows
        // the slot reclaimed.
        let _ = self.scopes.remove(&id);
        let capacity = self.scopes.capacity_report();
        let report = ScopedRequestReport::new(cancel_report, capacity);
        {
            let mut obs = self.obs.lock().expect("obs");
            match cause {
                ScopeCancelCause::Timeout => obs.timeout_reports.push(report),
                _ => obs.disconnect_reports.push(report),
            }
        }
        // Answer the caller if we still owe a reply (timeout path); a
        // disconnected caller is gone, so the request context is dropped.
        let answer = match (reply, tree.req.take()) {
            (Some(response), Some(req)) => reply_to(req, response),
            _ => noop(),
        };
        batch(vec![cancel_effect, answer])
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

// ---- Host: drive one clean upload + one mid-body disconnect. ----

/// Run the specimen: drive one clean streamed upload and one mid-body
/// disconnect, then report what the request tree did.
pub fn run() -> anyhow::Result<ScopedTreeReport> {
    let obs = Arc::new(Mutex::new(TreeObs::default()));
    let runtime = ThreadedRuntime::try_with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    )?;
    let enrich = runtime
        .register_with_capacity::<_, Infallible>(Enrich::default(), 8)
        .map_err(|e| anyhow::anyhow!("register enrich: {e:?}"))?;
    let controller = runtime
        .register_with_capacity::<_, Infallible>(
            Tree {
                enrich,
                scopes: RequestScopeSet::with_capacity(SCOPE_SET_CAPACITY),
                timers: ScopedTimerSet::with_capacity(TIMER_CAP),
                pending: HashMap::new(),
                next_id: 1,
                obs: obs.clone(),
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register controller: {e:?}"))?;

    let limits = HttpLimits {
        inbound_stream_chunk_size: Some(CHUNK_SIZE),
        ..HttpLimits::default()
    };
    let bind: SocketAddr = "127.0.0.1:0".parse()?;
    type TreeMessage = tina::ServiceMessage<TreeEvent, TreeRequest>;
    let listener = runtime
        .register_with_capacity::<HttpListener<SingleShard, TreeMessage>, _>(
            HttpListener::<SingleShard, TreeMessage>::new(
                bind,
                controller,
                limits,
                Duration::from_secs(5),
                16,
            ),
            8,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;
    let bound = runtime.observe_next_bound();
    runtime
        .try_send(listener, HttpListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start listener: {e:?}"))?;
    let addr = bound
        .wait(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("bind listener: {e:?}"))?;

    // 1. Clean streamed upload: full body, expect 200.
    let happy_body = b"hello-tree-ok";
    let happy = upload_full(addr, happy_body)?;
    anyhow::ensure!(
        happy.starts_with("HTTP/1.1 200"),
        "happy upload expected 200, got: {happy}"
    );
    wait_for(Duration::from_secs(2), || {
        obs.lock().expect("obs").clean_completions >= 1
    });

    // 2. Mid-body disconnect: declare 64 bytes, send 8, drop the socket.
    upload_then_disconnect(addr, 64, 8)?;
    let saw_disconnect = wait_for(Duration::from_secs(2), || {
        !obs.lock().expect("obs").disconnect_reports.is_empty()
    });
    anyhow::ensure!(
        saw_disconnect,
        "controller never recorded a disconnect report"
    );

    // 3. Deadline timeout: send an incomplete body and hold the socket
    //    open. The request can never finish, so its live deadline fires
    //    (`ScopedTimerFire::Run`) and the user sees a 504. This is
    //    race-free: the request is structurally stuck until the deadline.
    let timeout_response = upload_incomplete_and_read(addr, 64, 8);
    let timeout_replied_504 = timeout_response.starts_with("HTTP/1.1 504");

    // 4. Wait for the tombstoned deadline timers to fire and be ignored.
    wait_for(Duration::from_secs(2), || {
        obs.lock().expect("obs").timers_ignored_late >= 1
    });

    let listener_shutdown = runtime
        .try_send(listener, HttpListenerMsg::Stop)
        .map_err(|error| anyhow::anyhow!("stop listener: {error:?}"));
    let runtime_shutdown = runtime.shutdown_report().ensure_clean();
    listener_shutdown?;
    runtime_shutdown?;

    let snapshot = obs.lock().expect("obs");
    let disconnect = snapshot
        .disconnect_reports
        .first()
        .ok_or_else(|| anyhow::anyhow!("no disconnect report captured"))?;
    let cancelled = disconnect.cancelled_children();
    let enrich_cancel_ack_cancelled = snapshot
        .child_cancel_acks
        .iter()
        .any(|(label, outcome)| *label == "enrich" && *outcome == CancelOutcome::Cancelled);
    let replay_fact_line = scoped_tree_replay_line(64, 8, cancelled, 1)?;

    Ok(ScopedTreeReport {
        clean_completion: snapshot.clean_completions >= 1,
        disconnect_cancelled_children: cancelled,
        disconnect_cause_client: disconnect.cause() == ScopeCancelCause::ClientDisconnect,
        disconnect_report_clean: disconnect.is_clean(),
        timers_ignored_late: snapshot.timers_ignored_late,
        scope_capacity_reclaimed: disconnect.unreleased_capacity() == 0,
        timeout_replied_504,
        late_results: snapshot.late_results,
        enrich_cancel_ack_cancelled,
        disconnect_report_line: disconnect.summary_line(),
        replay_fact_line,
    })
}

fn upload_full(addr: SocketAddr, body: &[u8]) -> anyhow::Result<String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut request = format!(
        "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    stream.write_all(&request)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    Ok(String::from_utf8_lossy(&response).into_owned())
}

fn upload_then_disconnect(
    addr: SocketAddr,
    declared: usize,
    send_bytes: usize,
) -> anyhow::Result<()> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&vec![b'x'; send_bytes])?;
    stream.flush()?;
    // Drop the socket mid-body: the server's body pull sees a short read.
    drop(stream);
    Ok(())
}

/// Send an incomplete body and keep the socket open. The request can never
/// complete, so its deadline fires `Run` and the server writes a 504.
/// Returns whatever the server wrote (tolerating an early close).
fn upload_incomplete_and_read(addr: SocketAddr, declared: usize, send_bytes: usize) -> String {
    let connect = TcpStream::connect_timeout(&addr, Duration::from_secs(2));
    let Ok(mut stream) = connect else {
        return String::new();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_err()
        || stream.write_all(&vec![b'x'; send_bytes]).is_err()
        || stream.flush().is_err()
    {
        return String::new();
    }
    // Hold the socket open with the body incomplete; read whatever the
    // server sends (the 504). Tolerate an early close after the response.
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

fn wait_for(total: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

// ---- Sim/replay agreement for the request-scope-set surface. ----

#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeReplayOp {
    Disconnect {
        declared: usize,
        delivered: usize,
        cancelled_children: usize,
        peak_scopes: usize,
    },
}

/// Build the request-scope-set capacity fact and prove it round-trips
/// through capture/replay, or fail closed.
fn scoped_tree_replay_line(
    declared: usize,
    delivered: usize,
    cancelled_children: usize,
    peak_scopes: usize,
) -> anyhow::Result<String> {
    let op = TreeReplayOp::Disconnect {
        declared,
        delivered,
        cancelled_children,
        peak_scopes,
    };
    let case = DstReplayCase::new(
        "scoped_request_tree_disconnect",
        91,
        ReplayConfig::default(),
        "streaming body disconnect cancels the request scope",
        vec![op],
        "client disconnect cancels the request scope's pending children",
    );
    let fact = scope_set_fact(peak_scopes);

    let runner = |case: &DstReplayCase<TreeReplayOp>| {
        let Some(TreeReplayOp::Disconnect { peak_scopes, .. }) = case.history.operations().first()
        else {
            return Err(TraceProjectionError {
                event_id: EventId::new(0),
                kind: None,
                reason: "missing materialized replay op".to_owned(),
            });
        };
        let report = ReplayReport::from_case_and_events(case, &[] as &[RuntimeEvent], 0);
        Ok(LiveReplayReport::exact(report).with_live_fact(scope_set_fact(*peak_scopes)))
    };

    let report = runner(&case).expect("local live replay runner uses exact projection");
    let capture =
        LiveReplayCapture::from_case_and_report(&case, "scoped_request_tree_live", &report.replay)
            .with_live_fact(fact);
    check_captured_replay(&capture, &capture.to_replay_case(), runner)
        .map_err(|e| anyhow::anyhow!("live replay capture mismatch: {e}"))?;
    Ok(format!(
        "case=scoped_request_tree_disconnect declared={declared} delivered={delivered} \
         cancelled_children={cancelled_children} scope_set_cap={SCOPE_SET_CAPACITY} \
         fact=request_scope_set"
    ))
}

fn scope_set_fact(peak_scopes: usize) -> LiveReplayFact {
    LiveReplayFact::capacity_surface(&CapacitySurfaceReport::count(
        "scoped_request_tree.request_scope_set",
        CapacityMode::Fixed,
        SCOPE_SET_CAPACITY,
        0,
        peak_scopes,
        0,
    ))
}
