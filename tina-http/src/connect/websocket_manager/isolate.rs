//! The [`WebSocketClientManager`] isolate.
//!
//! It owns a bounded pool of [`WebSocketClientConnection`] isolates and one
//! current session. A `Connect` runs the bounded [`ConnectAttempts`] race
//! over resolved addresses; the winner becomes the session, every loser is
//! cancelled and stopped. A `Connect` after a session has ended is a bounded
//! reconnect. Old-generation replies are ignored by the generation guard.

use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;

use tina::prelude::*;
use tina::{Address, CallContext, CancelOutcome, RequestContext, Shard, reply_to_request};
use tina_runtime::{
    CallError, CallGroupToken, CallOutcome, ProtocolFact, RuntimeCall, ThreadedRuntime,
    ThreadedRuntimeError, call, call_cancelable, cancel_call, dns_lookup, sleep,
};

use crate::connect::attempts::{ConnectAttempts, ConnectStep};
use crate::connect::endpoint::{EndpointGeneration, EndpointId, WebSocketEndpoint};
use crate::connect::report::{ConnectAttemptOutcome, ConnectReport};
use crate::websocket::WebSocketMessage;
use crate::websocket_client::{
    WebSocketClientConnection, WebSocketClientError, WebSocketClientEvent, WebSocketClientMsg,
    WebSocketClientReply,
};

use super::WebSocketManagerConfig;
use super::state::{SessionEndReason, WebSocketManagerReport, WebSocketManagerState};

/// Address of one pooled WebSocket connection isolate.
pub type WsConnAddr = Address<WebSocketClientMsg, WebSocketClientReply>;

/// Address of a [`WebSocketClientManager`].
pub type WebSocketManagerAddr = Address<WebSocketManagerMsg, WebSocketManagerReply>;

/// Handles returned by [`build_websocket_client_manager`].
#[derive(Debug, Clone)]
pub struct WebSocketManagerHandles {
    /// The manager address.
    pub manager: WebSocketManagerAddr,
    /// One address per pooled connection isolate.
    pub connections: Vec<WsConnAddr>,
}

/// Build and register a [`WebSocketClientManager`] plus its bounded pool of
/// connection isolates.
///
/// Registers [`WebSocketClientManager::pool_size`] connection isolates so the
/// manager has enough idle slots to race the configured concurrency and hold
/// the session, then registers the manager over them.
pub fn build_websocket_client_manager<S>(
    runtime: &ThreadedRuntime<S, tina_runtime::DefaultThreadedMailboxFactory>,
    endpoint: WebSocketEndpoint,
    config: WebSocketManagerConfig,
    manager_mailbox_capacity: usize,
    connection_mailbox_capacity: usize,
) -> Result<WebSocketManagerHandles, ThreadedRuntimeError>
where
    S: Shard + Send + 'static,
{
    let pool = WebSocketClientManager::<S>::pool_size(&config);
    let mut connections: Vec<WsConnAddr> = Vec::with_capacity(pool);
    for _ in 0..pool {
        let conn = WebSocketClientConnection::<S>::new(config.session_limits);
        let address = runtime
            .register_with_capacity::<WebSocketClientConnection<S>, Infallible>(
                conn,
                connection_mailbox_capacity,
            )?;
        connections.push(address);
    }
    let manager = WebSocketClientManager::<S>::new(endpoint, config, connections.clone());
    let manager_address = runtime
        .register_with_capacity::<WebSocketClientManager<S>, WebSocketClientMsg>(
            manager,
            manager_mailbox_capacity,
        )?;
    Ok(WebSocketManagerHandles {
        manager: manager_address,
        connections,
    })
}

/// Typed outcome of a manager `Connect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketConnectOutcome {
    /// A session opened. The report names the winner and the losers.
    Connected(ConnectReport),
    /// Every connect attempt failed.
    ConnectFailed(ConnectReport),
    /// The reconnect budget is exhausted; no healthy endpoint remains.
    NoHealthyEndpoint(ConnectReport),
    /// Every attempt failed with a connect timeout.
    TimedOut(ConnectReport),
    /// A connect is already in progress, or no connection slot is free.
    Full,
    /// A session is already open at this generation.
    AlreadyConnected(EndpointGeneration),
}

/// Why a session operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketSessionError {
    /// No session is open.
    NotConnected,
    /// The session closed.
    Closed,
    /// A session operation is already in flight.
    Busy,
    /// The underlying session reported a protocol/transport error.
    Session(WebSocketClientError),
}

/// Reply from a [`WebSocketClientManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketManagerReply {
    /// Result of a `Connect`.
    Connect(WebSocketConnectOutcome),
    /// Result of a `Send`.
    Sent(Result<(), WebSocketSessionError>),
    /// Result of a `Receive`.
    Event(Result<WebSocketClientEvent, WebSocketSessionError>),
    /// A manager state report.
    Report(WebSocketManagerReport),
    /// The current session was closed.
    Closed,
    /// The manager drained and stopped.
    Shutdown(WebSocketManagerShutdownReport),
}

/// Result of a manager shutdown drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketManagerShutdownReport {
    /// Connection isolates asked to stop (sessions + in-flight dials).
    pub stopped: usize,
    /// Connect attempts cancelled mid-race.
    pub attempts_cancelled: usize,
    /// Final manager state at shutdown.
    pub state: WebSocketManagerReport,
}

/// Messages accepted by a [`WebSocketClientManager`].
#[derive(Debug)]
pub enum WebSocketManagerMsg {
    /// Open (or reconnect) the session.
    Connect,
    /// Send one message on the current session.
    Send(WebSocketMessage),
    /// Pull one event from the current session.
    Receive,
    /// Read a manager report (refreshes current-session pressure).
    Report,
    /// Close the current session.
    Close,
    /// Drain and stop the manager.
    Shutdown,
    // ---- continuations (handle lane) ----
    /// DNS lookup completion.
    Dns(Result<Vec<SocketAddr>, CallError>),
    /// One connect attempt completion.
    AttemptReply {
        /// Dialed address.
        addr: SocketAddr,
        /// Branch token.
        token: CallGroupToken,
        /// Connect-call outcome.
        outcome: CallOutcome<WebSocketClientReply>,
    },
    /// One loser-cancel completion.
    CancelDone {
        /// Loser address.
        addr: SocketAddr,
        /// Branch token.
        token: CallGroupToken,
        /// Cancel outcome.
        outcome: CancelOutcome,
    },
    /// Happy Eyeballs stagger tick for the connect at this generation.
    StaggerTick {
        /// Generation the tick belongs to.
        generation: EndpointGeneration,
    },
    /// A routed `Send` completed.
    SessionSent {
        /// Session generation that handled it.
        generation: EndpointGeneration,
        /// Session reply.
        outcome: CallOutcome<WebSocketClientReply>,
    },
    /// A routed `Receive` completed.
    SessionEvent {
        /// Session generation that handled it.
        generation: EndpointGeneration,
        /// Session reply.
        outcome: CallOutcome<WebSocketClientReply>,
    },
    /// A session pressure `Report` completed.
    SessionReport {
        /// Session generation that handled it.
        generation: EndpointGeneration,
        /// Session reply.
        outcome: CallOutcome<WebSocketClientReply>,
    },
}

/// Per connection-pool slot state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    Idle,
    Dialing {
        generation: EndpointGeneration,
        addr: SocketAddr,
    },
    Session {
        generation: EndpointGeneration,
    },
}

/// Reconnecting WebSocket client manager over a bounded connection pool.
pub struct WebSocketClientManager<S: Shard + 'static> {
    endpoint: WebSocketEndpoint,
    config: WebSocketManagerConfig,
    connections: Vec<WsConnAddr>,
    conn_states: Vec<ConnState>,
    state: WebSocketManagerState,
    connect: Option<ConnectAttempts<WebSocketClientReply>>,
    started_once: bool,
    pending_connect: Option<RequestContext<WebSocketManagerReply>>,
    pending_send: Option<RequestContext<WebSocketManagerReply>>,
    pending_receive: Option<RequestContext<WebSocketManagerReply>>,
    pending_report: Option<RequestContext<WebSocketManagerReply>>,
    _shard: PhantomData<S>,
}

impl<S: Shard + 'static> WebSocketClientManager<S> {
    /// Build a manager for one endpoint over a pre-registered connection
    /// pool. The pool must have at least
    /// `max(max_sessions, happy_eyeballs.max_concurrent_attempts)` slots.
    pub fn new(
        endpoint: WebSocketEndpoint,
        config: WebSocketManagerConfig,
        connections: Vec<WsConnAddr>,
    ) -> Self {
        let pool = connections.len();
        let state = WebSocketManagerState::new(
            EndpointId::new(1),
            config.max_sessions,
            config.max_reconnects,
            config.retained_reports,
        );
        Self {
            endpoint,
            config,
            conn_states: vec![ConnState::Idle; pool],
            connections,
            state,
            connect: None,
            started_once: false,
            pending_connect: None,
            pending_send: None,
            pending_receive: None,
            pending_report: None,
            _shard: PhantomData,
        }
    }

    /// Pool size needed for this config: enough idle slots to race the
    /// configured concurrency and still hold the sessions.
    pub fn pool_size(config: &WebSocketManagerConfig) -> usize {
        config
            .max_sessions
            .max(config.connect_policy.happy_eyeballs.max_concurrent_attempts)
    }
}

impl<S: Shard + 'static> Isolate for WebSocketClientManager<S> {
    tina::isolate_types! {
        message: WebSocketManagerMsg,
        reply: WebSocketManagerReply,
        send: tina::Outbound<WebSocketClientMsg>,
        spawn: Infallible,
        call: RuntimeCall<WebSocketManagerMsg>,
        fact: ProtocolFact,
        shard: S,
    }

    fn handle(
        &mut self,
        msg: WebSocketManagerMsg,
        _ctx: &mut Context<'_, S, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WebSocketManagerMsg::Dns(result) => self.on_dns(result),
            WebSocketManagerMsg::AttemptReply {
                addr,
                token,
                outcome,
            } => self.on_attempt(addr, token, outcome),
            WebSocketManagerMsg::CancelDone {
                addr,
                token,
                outcome,
            } => self.on_cancel(addr, token, outcome),
            WebSocketManagerMsg::StaggerTick { generation } => self.on_stagger(generation),
            WebSocketManagerMsg::SessionSent {
                generation,
                outcome,
            } => self.on_session_sent(generation, outcome),
            WebSocketManagerMsg::SessionEvent {
                generation,
                outcome,
            } => self.on_session_event(generation, outcome),
            WebSocketManagerMsg::SessionReport {
                generation,
                outcome,
            } => self.on_session_report(generation, outcome),
            // Call-lane messages delivered fire-and-forget are caller misuse.
            WebSocketManagerMsg::Connect
            | WebSocketManagerMsg::Send(_)
            | WebSocketManagerMsg::Receive
            | WebSocketManagerMsg::Report
            | WebSocketManagerMsg::Close
            | WebSocketManagerMsg::Shutdown => noop(),
        }
    }

    fn handle_call(
        &mut self,
        msg: WebSocketManagerMsg,
        call_ctx: CallContext<'_, Self>,
    ) -> Effect<Self> {
        match msg {
            WebSocketManagerMsg::Connect => self.on_connect_request(call_ctx),
            WebSocketManagerMsg::Send(message) => self.on_send_request(message, call_ctx),
            WebSocketManagerMsg::Receive => self.on_receive_request(call_ctx),
            WebSocketManagerMsg::Report => self.on_report_request(call_ctx),
            WebSocketManagerMsg::Close => self.on_close_request(call_ctx),
            WebSocketManagerMsg::Shutdown => self.on_shutdown_request(call_ctx),
            _ => call_ctx.reject(tina::CallRejectedReason::UnsupportedMessage),
        }
    }
}

impl<S: Shard + 'static> WebSocketClientManager<S> {
    // ---- Connect ----

    fn on_connect_request(&mut self, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        if let Some(generation) = self.state.current_generation() {
            return call_ctx.reply(WebSocketManagerReply::Connect(
                WebSocketConnectOutcome::AlreadyConnected(generation),
            ));
        }
        if self.connect.is_some() || self.pending_connect.is_some() {
            return call_ctx
                .reply(WebSocketManagerReply::Connect(WebSocketConnectOutcome::Full));
        }
        // A connect after the first is a reconnect: spend the bounded budget.
        if self.started_once && !self.state.record_reconnect() {
            let report = self.minimal_report(DnsForcedFail::NoHealthy);
            self.state.retain_failed_connect(EndpointGeneration::new(0), true);
            return call_ctx.reply(WebSocketManagerReply::Connect(
                WebSocketConnectOutcome::NoHealthyEndpoint(report),
            ));
        }
        self.started_once = true;
        let generation = self.state.begin_generation();
        self.connect = Some(ConnectAttempts::new(
            self.state_endpoint_id(),
            generation,
            self.endpoint.host(),
            self.endpoint.port(),
            self.endpoint.authority(),
            &self.endpoint.connect_security(),
            self.config.connect_policy,
        ));
        self.pending_connect = Some(call_ctx.into_request_context());
        dns_lookup(
            self.endpoint.host().to_string(),
            self.endpoint.port(),
            self.config.connect_policy.dns_timeout,
        )
        .then(WebSocketManagerMsg::Dns)
    }

    fn on_dns(&mut self, result: Result<Vec<SocketAddr>, CallError>) -> Effect<Self> {
        let Some(connect) = self.connect.as_mut() else {
            return noop();
        };
        match connect.record_dns(result) {
            crate::connect::attempts::DnsClassification::Proceed => self.admit_attempts(),
            crate::connect::attempts::DnsClassification::NoAddresses
            | crate::connect::attempts::DnsClassification::Failed => self.finish_failed_connect(),
        }
    }

    /// Start as many attempts as the concurrency cap allows now. With a
    /// non-zero Happy Eyeballs delay, start one and schedule a stagger tick;
    /// otherwise start the full first wave at once.
    fn admit_attempts(&mut self) -> Effect<Self> {
        let generation = match self.connect.as_ref() {
            Some(c) => c.generation(),
            None => return noop(),
        };
        let stagger = !self.config.connect_policy.happy_eyeballs.delay.is_zero();
        let mut effects: Vec<Effect<Self>> = Vec::new();
        // Only take a candidate when a connection slot is free, so a popped
        // candidate is never dropped for want of a slot.
        while self.idle_slot().is_some() {
            let Some(addr) = self.connect.as_mut().and_then(|c| c.take_candidate()) else {
                break;
            };
            match self.start_attempt(addr, generation) {
                Some(effect) => effects.push(effect),
                None => break,
            }
            if stagger {
                // Stagger: one now, schedule the next.
                if self
                    .connect
                    .as_ref()
                    .is_some_and(|c| c.candidates_remaining() > 0)
                {
                    effects.push(
                        sleep(self.config.connect_policy.happy_eyeballs.delay)
                            .then(move |_| WebSocketManagerMsg::StaggerTick { generation }),
                    );
                }
                break;
            }
        }
        batch(effects)
    }

    fn on_stagger(&mut self, generation: EndpointGeneration) -> Effect<Self> {
        // Only the connect in progress at this generation may admit more.
        if self
            .connect
            .as_ref()
            .is_none_or(|c| c.generation() != generation)
        {
            return noop();
        }
        self.admit_attempts()
    }

    fn start_attempt(
        &mut self,
        addr: SocketAddr,
        generation: EndpointGeneration,
    ) -> Option<Effect<Self>> {
        let slot = self.idle_slot()?;
        let conn = self.connections[slot];
        let target = self.endpoint.resolve(addr);
        let subprotocols = self.config.subprotocols.clone();
        let timeout = self.config.connect_policy.connect_timeout;
        let connect = self.connect.as_mut()?;
        let call = call_cancelable(
            conn,
            WebSocketClientMsg::Connect {
                target,
                subprotocols,
            },
            timeout,
        );
        match connect.start::<Self, _, _, _>(addr, call, |addr, token, outcome| {
            WebSocketManagerMsg::AttemptReply {
                addr,
                token,
                outcome,
            }
        }) {
            Ok(effect) => {
                self.conn_states[slot] = ConnState::Dialing { generation, addr };
                Some(effect)
            }
            Err(_) => None,
        }
    }

    fn on_attempt(
        &mut self,
        addr: SocketAddr,
        token: CallGroupToken,
        outcome: CallOutcome<WebSocketClientReply>,
    ) -> Effect<Self> {
        let Some(connect) = self.connect.as_mut() else {
            return noop();
        };
        let step = connect.record_attempt(addr, token, outcome, classify_ws);
        match step {
            ConnectStep::Won { losers } => {
                let winner_slot = self.slot_dialing(addr);
                let mut effects: Vec<Effect<Self>> = Vec::new();
                if let Some(slot) = winner_slot {
                    let generation = connect_generation(self.connect.as_ref());
                    match self.state.install_session(generation, slot, addr) {
                        Ok(()) => {
                            self.conn_states[slot] = ConnState::Session { generation };
                        }
                        Err(_) => {
                            // Could not install (slot full / stale): the
                            // connection is open, so stop it to release the
                            // stream rather than leak it.
                            effects.push(send(self.connections[slot], WebSocketClientMsg::Stop));
                            self.conn_states[slot] = ConnState::Idle;
                        }
                    }
                }
                for loser in losers {
                    let (laddr, ltoken, handle) = loser.into_parts();
                    if let Some(slot) = self.slot_dialing(laddr) {
                        effects.push(send(self.connections[slot], WebSocketClientMsg::Stop));
                        self.conn_states[slot] = ConnState::Idle;
                    }
                    effects.push(cancel_call(handle).then(move |outcome| {
                        WebSocketManagerMsg::CancelDone {
                            addr: laddr,
                            token: ltoken,
                            outcome,
                        }
                    }));
                }
                if self.connect.as_ref().is_some_and(|c| c.is_settled()) {
                    effects.push(self.finish_connected());
                }
                batch(effects)
            }
            ConnectStep::Continue => {
                if let Some(slot) = self.slot_dialing(addr) {
                    self.conn_states[slot] = ConnState::Idle;
                }
                self.admit_attempts()
            }
            ConnectStep::Exhausted => {
                if let Some(slot) = self.slot_dialing(addr) {
                    self.conn_states[slot] = ConnState::Idle;
                }
                self.finish_failed_connect()
            }
            ConnectStep::LateCompletion { addr, connected } => {
                if let Some(slot) = self.slot_dialing(addr) {
                    self.conn_states[slot] = ConnState::Idle;
                    if connected {
                        // The late winner connected: stop it to release the
                        // stream Tina now owns. It can never be the success.
                        return send(self.connections[slot], WebSocketClientMsg::Stop);
                    }
                }
                noop()
            }
            ConnectStep::Settled => self.finish_connected(),
        }
    }

    fn on_cancel(
        &mut self,
        addr: SocketAddr,
        token: CallGroupToken,
        outcome: CancelOutcome,
    ) -> Effect<Self> {
        let Some(connect) = self.connect.as_mut() else {
            return noop();
        };
        if let ConnectStep::Settled = connect.record_cancel(addr, token, outcome) {
            return self.finish_connected();
        }
        noop()
    }

    fn finish_connected(&mut self) -> Effect<Self> {
        let Some(connect) = self.connect.take() else {
            return noop();
        };
        let report = connect.into_report();
        match self.pending_connect.take() {
            Some(req) => reply_to_request(
                req,
                WebSocketManagerReply::Connect(WebSocketConnectOutcome::Connected(report)),
            ),
            None => noop(),
        }
    }

    fn finish_failed_connect(&mut self) -> Effect<Self> {
        let Some(connect) = self.connect.take() else {
            return noop();
        };
        let generation = connect.generation();
        let report = connect.into_report();
        let all_timeout = !report.attempted.is_empty()
            && report
                .attempted
                .iter()
                .all(|a| a.outcome == ConnectAttemptOutcome::ConnectTimeout);
        self.state.retain_failed_connect(generation, false);
        let outcome = if all_timeout {
            WebSocketConnectOutcome::TimedOut(report)
        } else {
            WebSocketConnectOutcome::ConnectFailed(report)
        };
        match self.pending_connect.take() {
            Some(req) => reply_to_request(req, WebSocketManagerReply::Connect(outcome)),
            None => noop(),
        }
    }

    // ---- Send / Receive / Report / Close ----

    fn on_send_request(
        &mut self,
        message: WebSocketMessage,
        call_ctx: CallContext<'_, Self>,
    ) -> Effect<Self> {
        let Some(generation) = self.state.current_generation() else {
            return call_ctx.reply(WebSocketManagerReply::Sent(Err(
                WebSocketSessionError::NotConnected,
            )));
        };
        if self.pending_send.is_some() {
            return call_ctx.reply(WebSocketManagerReply::Sent(Err(WebSocketSessionError::Busy)));
        }
        let Some(slot) = self.state.current_conn_index() else {
            return call_ctx.reply(WebSocketManagerReply::Sent(Err(
                WebSocketSessionError::NotConnected,
            )));
        };
        self.pending_send = Some(call_ctx.into_request_context());
        call(
            self.connections[slot],
            WebSocketClientMsg::Send(message),
            self.config.connect_policy.connect_timeout,
        )
        .then(move |outcome| WebSocketManagerMsg::SessionSent {
            generation,
            outcome,
        })
    }

    fn on_session_sent(
        &mut self,
        generation: EndpointGeneration,
        outcome: CallOutcome<WebSocketClientReply>,
    ) -> Effect<Self> {
        if !self.state.is_current_session(generation) {
            self.state.note_stale_reply();
            return self.reply_send(Err(WebSocketSessionError::Closed));
        }
        match outcome {
            CallOutcome::Replied(WebSocketClientReply::Sent(Ok(()))) => {
                self.reply_send(Ok(()))
            }
            CallOutcome::Replied(WebSocketClientReply::Sent(Err(error))) => {
                let mapped = self.map_session_error(generation, error);
                self.reply_send(Err(mapped))
            }
            CallOutcome::Replied(_) => self.reply_send(Err(WebSocketSessionError::Closed)),
            CallOutcome::Timeout => self.reply_send(Err(WebSocketSessionError::Closed)),
            CallOutcome::Full | CallOutcome::Closed | CallOutcome::Rejected(_) => {
                self.retire_current(SessionEndReason::ClosedByPeer);
                self.reply_send(Err(WebSocketSessionError::Closed))
            }
        }
    }

    fn reply_send(&mut self, result: Result<(), WebSocketSessionError>) -> Effect<Self> {
        match self.pending_send.take() {
            Some(req) => reply_to_request(req, WebSocketManagerReply::Sent(result)),
            None => noop(),
        }
    }

    fn on_receive_request(&mut self, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        let Some(generation) = self.state.current_generation() else {
            return call_ctx.reply(WebSocketManagerReply::Event(Err(
                WebSocketSessionError::NotConnected,
            )));
        };
        if self.pending_receive.is_some() {
            return call_ctx.reply(WebSocketManagerReply::Event(Err(WebSocketSessionError::Busy)));
        }
        let Some(slot) = self.state.current_conn_index() else {
            return call_ctx.reply(WebSocketManagerReply::Event(Err(
                WebSocketSessionError::NotConnected,
            )));
        };
        self.pending_receive = Some(call_ctx.into_request_context());
        call(
            self.connections[slot],
            WebSocketClientMsg::Receive,
            self.config.connect_policy.connect_timeout,
        )
        .then(move |outcome| WebSocketManagerMsg::SessionEvent {
            generation,
            outcome,
        })
    }

    fn on_session_event(
        &mut self,
        generation: EndpointGeneration,
        outcome: CallOutcome<WebSocketClientReply>,
    ) -> Effect<Self> {
        if !self.state.is_current_session(generation) {
            self.state.note_stale_reply();
            return self.reply_receive(Err(WebSocketSessionError::Closed));
        }
        match outcome {
            CallOutcome::Replied(WebSocketClientReply::Event(Ok(event))) => {
                if let WebSocketClientEvent::Close { .. } = &event {
                    // Peer close: retire the session so the next Connect is a
                    // bounded reconnect at a new generation.
                    self.retire_current(SessionEndReason::ClosedByPeer);
                }
                self.reply_receive(Ok(event))
            }
            CallOutcome::Replied(WebSocketClientReply::Event(Err(error))) => {
                let mapped = self.map_session_error(generation, error);
                self.reply_receive(Err(mapped))
            }
            CallOutcome::Replied(_) => self.reply_receive(Err(WebSocketSessionError::Closed)),
            CallOutcome::Timeout => self.reply_receive(Err(WebSocketSessionError::Closed)),
            CallOutcome::Full | CallOutcome::Closed | CallOutcome::Rejected(_) => {
                self.retire_current(SessionEndReason::ClosedByPeer);
                self.reply_receive(Err(WebSocketSessionError::Closed))
            }
        }
    }

    fn reply_receive(
        &mut self,
        result: Result<WebSocketClientEvent, WebSocketSessionError>,
    ) -> Effect<Self> {
        match self.pending_receive.take() {
            Some(req) => reply_to_request(req, WebSocketManagerReply::Event(result)),
            None => noop(),
        }
    }

    fn on_report_request(&mut self, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        // Refresh current-session pressure when a session is open.
        if let (Some(generation), Some(slot)) = (
            self.state.current_generation(),
            self.state.current_conn_index(),
        ) {
            if self.pending_report.is_none() {
                self.pending_report = Some(call_ctx.into_request_context());
                return call(
                    self.connections[slot],
                    WebSocketClientMsg::Report,
                    self.config.connect_policy.connect_timeout,
                )
                .then(move |outcome| WebSocketManagerMsg::SessionReport {
                    generation,
                    outcome,
                });
            }
        }
        call_ctx.reply(WebSocketManagerReply::Report(self.state.report()))
    }

    fn on_session_report(
        &mut self,
        generation: EndpointGeneration,
        outcome: CallOutcome<WebSocketClientReply>,
    ) -> Effect<Self> {
        if let CallOutcome::Replied(WebSocketClientReply::Report(report)) = outcome {
            self.state.record_pressure(generation, report);
        }
        match self.pending_report.take() {
            Some(req) => reply_to_request(req, WebSocketManagerReply::Report(self.state.report())),
            None => noop(),
        }
    }

    fn on_close_request(&mut self, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        if let Some(slot) = self.state.current_conn_index() {
            let stop = send(self.connections[slot], WebSocketClientMsg::Stop);
            self.retire_current(SessionEndReason::ClosedLocal);
            return batch(vec![stop, call_ctx.reply(WebSocketManagerReply::Closed)]);
        }
        call_ctx.reply(WebSocketManagerReply::Closed)
    }

    fn on_shutdown_request(&mut self, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        let mut effects: Vec<Effect<Self>> = Vec::new();
        let mut stopped = 0usize;
        // Stop every live connection (session or dialing).
        for slot in 0..self.connections.len() {
            if !matches!(self.conn_states[slot], ConnState::Idle) {
                effects.push(send(self.connections[slot], WebSocketClientMsg::Stop));
                self.conn_states[slot] = ConnState::Idle;
                stopped += 1;
            }
        }
        // Cancel any in-flight connect race.
        let mut attempts_cancelled = 0usize;
        if let Some(connect) = self.connect.as_mut() {
            for loser in connect.drain_for_cancel() {
                let (laddr, ltoken, handle) = loser.into_parts();
                attempts_cancelled += 1;
                effects.push(cancel_call(handle).then(move |outcome| {
                    WebSocketManagerMsg::CancelDone {
                        addr: laddr,
                        token: ltoken,
                        outcome,
                    }
                }));
            }
        }
        self.connect = None;
        if self.state.has_session() {
            self.state.retire_current(SessionEndReason::ClosedLocal);
        }
        let report = WebSocketManagerShutdownReport {
            stopped,
            attempts_cancelled,
            state: self.state.report(),
        };
        effects.push(call_ctx.reply(WebSocketManagerReply::Shutdown(report)));
        batch(effects)
    }

    // ---- helpers ----

    fn retire_current(&mut self, reason: SessionEndReason) {
        if let Some(slot) = self.state.retire_current(reason) {
            if slot < self.conn_states.len() {
                self.conn_states[slot] = ConnState::Idle;
            }
        }
    }

    fn map_session_error(
        &mut self,
        generation: EndpointGeneration,
        error: WebSocketClientError,
    ) -> WebSocketSessionError {
        match error {
            WebSocketClientError::Closed | WebSocketClientError::NotConnected => {
                if self.state.is_current_session(generation) {
                    self.retire_current(SessionEndReason::ClosedByPeer);
                }
                WebSocketSessionError::Closed
            }
            WebSocketClientError::Busy => WebSocketSessionError::Busy,
            other => WebSocketSessionError::Session(other),
        }
    }

    fn idle_slot(&self) -> Option<usize> {
        self.conn_states
            .iter()
            .position(|s| matches!(s, ConnState::Idle))
    }

    fn slot_dialing(&self, addr: SocketAddr) -> Option<usize> {
        self.conn_states.iter().position(|s| {
            matches!(s, ConnState::Dialing { addr: a, .. } if *a == addr)
        })
    }

    fn state_endpoint_id(&self) -> EndpointId {
        EndpointId::new(1)
    }

    fn minimal_report(&self, _forced: DnsForcedFail) -> ConnectReport {
        ConnectReport {
            endpoint: self.state_endpoint_id(),
            generation: self
                .state
                .current_generation()
                .unwrap_or(EndpointGeneration::new(0)),
            host: self.endpoint.host().to_string(),
            port: self.endpoint.port(),
            authority: self.endpoint.authority(),
            tls: None,
            dns: crate::connect::report::DnsOutcome::NotAttempted,
            resolved_addresses: Vec::new(),
            attempted: Vec::new(),
            winner: None,
            cancelled_losers: 0,
            late_completions: 0,
        }
    }
}

enum DnsForcedFail {
    NoHealthy,
}

fn connect_generation(connect: Option<&ConnectAttempts<WebSocketClientReply>>) -> EndpointGeneration {
    connect
        .map(|c| c.generation())
        .unwrap_or(EndpointGeneration::new(0))
}

/// Classify a WebSocket connect reply into a typed attempt outcome.
fn classify_ws(reply: &WebSocketClientReply) -> ConnectAttemptOutcome {
    match reply {
        WebSocketClientReply::Connected(Ok(_)) => ConnectAttemptOutcome::Connected,
        WebSocketClientReply::Connected(Err(error)) => match error {
            WebSocketClientError::Tls(CallError::TlsCertificate) => {
                ConnectAttemptOutcome::TlsCertificate
            }
            WebSocketClientError::Tls(CallError::TlsName) => ConnectAttemptOutcome::TlsName,
            WebSocketClientError::Tls(CallError::TlsAlpnMismatch) => {
                ConnectAttemptOutcome::TlsAlpnMismatch
            }
            WebSocketClientError::Tls(CallError::TlsFull) => ConnectAttemptOutcome::TlsFull,
            WebSocketClientError::Tls(CallError::TlsClosed) => ConnectAttemptOutcome::TlsClosed,
            WebSocketClientError::Tls(CallError::Timeout) => ConnectAttemptOutcome::ConnectTimeout,
            WebSocketClientError::Tls(_) => ConnectAttemptOutcome::TlsHandshake,
            _ => ConnectAttemptOutcome::ConnectIo,
        },
        _ => ConnectAttemptOutcome::ConnectIo,
    }
}
