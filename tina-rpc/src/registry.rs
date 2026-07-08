//! Service registry isolate.
//!
//! Owns a service-name → service-isolate-address table. Connection isolates
//! forward each [`crate::RouterRequest`] to the registry; the registry looks
//! up the service, issues an `IsolateCall` to it with a uniform
//! `(method, payload)` envelope, and translates the service's reply back
//! into a [`crate::RouterReply`] for the connection.
//!
//! # Wire-error mapping (registry → connection → wire)
//!
//! | Service outcome              | RouterReply  | Wire frame the connection emits |
//! |------------------------------|--------------|---------------------------------|
//! | `Replied(Ok(bytes))`         | `Ok(bytes)`  | `Reply` with bytes              |
//! | `Replied(UnknownMethod)`     | `UnknownMethod` | `Error(UnknownMethod)`       |
//! | `Replied(Decode)`            | `Decode`     | `Error(Decode)`                 |
//! | `Replied(Internal)`          | `Internal`   | `Error(Internal)`               |
//! | `Full` (service mailbox full)| `Full`       | `Error(Full)`                   |
//! | `Closed` (service gone)      | `Internal`   | `Error(Internal)`               |
//! | `Timeout` (service stalled)  | `Internal`   | `Error(Internal)`               |
//!
//! # Why a separate envelope (`RegistryMsg`)
//!
//! Tina's `IsolateCall` requires the issuing isolate's translator to produce
//! the issuer's own `Self::Message` type. The registry both *receives*
//! external `RouterRequest`s (as `call()` traffic, on `handle_call`) and
//! *receives* internal continuation messages (the service-call results, on
//! `handle`). They share the registry's mailbox, so its message vocabulary is
//! an envelope:
//!
//! - `RegistryMsg::Route(RouterRequest)` — external entrypoint from a
//!   connection isolate, answered through the caller's `RequestContext`.
//! - `RegistryMsg::ServiceResult(RequestContext<RouterReply>,
//!   CallOutcome<ServiceReply>)` — internal continuation carrying the original
//!   caller's captured reply authority plus the service `IsolateCall` outcome.
//!
//! The connection calls only `Route(...)`. `ServiceResult` is an
//! implementation detail of the registry's deferred-reply mechanism.
//!
//! # Wire-error invariant
//!
//! Server timeouts on the service `IsolateCall` map to
//! `RouterReply::Internal` — *not* a fictional wire `Timeout` frame. The
//! plan-level rule is that `timeout` is a client-observed condition only.
//! The server's "didn't get a service reply in time" surfaces to the client
//! as `Internal`; the client's own deadline is what produces the `Timeout`
//! it actually observes.

use std::collections::HashMap;
use std::time::Duration;

use tina::prelude::*;
use tina::{CallContext, CallRejectedReason, CallableIsolate};
use tina_runtime::{CallOutcome, RuntimeCall, call};

use crate::connection::{RouterReply, RouterRequest};

/// Uniform envelope every registered service speaks.
///
/// Services are Tina isolates whose `Message` is `ServiceCall` and whose
/// `Reply` is [`ServiceReply`]. This uniform shape is what lets the
/// registry hold a typed `Address<ServiceCall, ServiceReply>` per service
/// regardless of what the service does internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCall {
    /// Method name from the request frame.
    pub method: String,
    /// Opaque request payload bytes.
    pub payload: Vec<u8>,
}

/// Reply type every registered service speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceReply {
    /// The service produced these bytes for the request.
    Ok(Vec<u8>),
    /// The service does not recognize the named method.
    UnknownMethod,
    /// The service rejected the payload as undecodable.
    Decode,
    /// The service hit an internal error unrelated to the payload.
    Internal,
}

/// Inbound vocabulary of the registry isolate.
///
/// External callers should only ever construct or send the
/// [`RegistryMsg::Route`] variant. The other variants are produced by the
/// registry's own `IsolateCall` translators and are public solely so the
/// `Self::Message` type used by the runtime stays expressible.
///
/// # In-process trust boundary
///
/// Constructing a [`RegistryMsg::ServiceResult`] from outside the registry
/// and routing it into the registry mailbox does *not* let an attacker
/// hijack a victim's pending reply: the carried [`RequestContext`] is a
/// move-only, non-forgeable capture of the *original* caller's reply slot,
/// minted by the runtime when the `Route` call was delivered. An in-process
/// actor cannot construct one for a victim's call.
///
/// Not `Clone`: the `ServiceResult` continuation owns a one-shot
/// [`RequestContext`], which is move-only by design.
#[derive(Debug)]
pub enum RegistryMsg {
    /// External request from a connection isolate. Delivered as a `call()`,
    /// so it is answered through [`RequestContext`] (captured in
    /// `handle_call`), never through an implicit reply slot.
    Route(RouterRequest),
    /// Internal continuation: a service `IsolateCall` completed.
    ///
    /// Carries the original caller's captured [`RequestContext`] plus the
    /// downstream call outcome. The registry maps the outcome to a
    /// [`RouterReply`] and answers the original caller with
    /// [`tina::reply_to`]. Produced only by the registry's own deferred-call
    /// continuation and delivered back through `handle`. It cannot be forged:
    /// a [`RequestContext`] is minted only by the runtime for a real caller.
    #[doc(hidden)]
    ServiceResult(RequestContext<RouterReply>, CallOutcome<ServiceReply>),
}

/// Registry configuration.
#[derive(Debug, Clone)]
pub struct RegistryConfig {
    /// Timeout the registry uses on every `IsolateCall` to a service.
    ///
    /// **Set this shorter than the connection's `service_call_timeout`**
    /// so the registry-side timeout fires first and the client sees a
    /// well-formed wire response (`Error(Internal)`) rather than the
    /// connection's own timeout firing and producing no wire frame at
    /// all (per the wire-error invariant). Inverting the relationship —
    /// connection timeout shorter than registry timeout — leaves the
    /// client to surface a local `Timeout` while the server still has
    /// the work in flight, which is correct but harder to debug.
    ///
    /// "Registry-dominant" is the recommended posture; the default 4 s
    /// pairs with the connection's default 5 s service-call timeout.
    pub service_call_timeout: Duration,
}

impl Default for RegistryConfig {
    /// 4 s default — slightly under the connection isolate's 5 s default
    /// `service_call_timeout` so the registry-side timeout dominates.
    fn default() -> Self {
        Self {
            service_call_timeout: Duration::from_secs(4),
        }
    }
}

/// The service registry isolate.
///
/// Generic over the shard type so applications can host the registry on
/// whatever shard they already use. The proc-macro path mishandles generic
/// `Self` types, so the [`tina::Isolate`] impl is hand-written.
pub struct Registry<S>
where
    S: tina::Shard,
{
    services: HashMap<String, Address<ServiceCall, ServiceReply>>,
    config: RegistryConfig,
    _shard: std::marker::PhantomData<S>,
}

impl<S> std::fmt::Debug for Registry<S>
where
    S: tina::Shard,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("services", &self.services.keys().collect::<Vec<_>>())
            .field("config", &self.config)
            .finish()
    }
}

impl<S> Registry<S>
where
    S: tina::Shard,
{
    /// Builds an empty registry with the given configuration.
    ///
    /// Panics in debug builds if `config.service_call_timeout` is zero;
    /// such a registry would have every service call resolve as
    /// `CallOutcome::Timeout` on the next tick and surface every request
    /// as `RouterReply::Internal` on the wire — almost never what the
    /// caller wants.
    pub fn new(config: RegistryConfig) -> Self {
        debug_assert!(
            !config.service_call_timeout.is_zero(),
            "RegistryConfig::service_call_timeout must be non-zero",
        );
        Self {
            services: HashMap::new(),
            config,
            _shard: std::marker::PhantomData,
        }
    }

    /// Registers `addr` as the service named `name`. Re-registering the same
    /// name overwrites the previous binding.
    pub fn register(&mut self, name: impl Into<String>, addr: Address<ServiceCall, ServiceReply>) {
        self.services.insert(name.into(), addr);
    }

    /// Removes a service registration. Returns the previous address if any.
    pub fn deregister(&mut self, name: &str) -> Option<Address<ServiceCall, ServiceReply>> {
        self.services.remove(name)
    }

    /// Returns the number of currently-registered services.
    pub fn len(&self) -> usize {
        self.services.len()
    }

    /// True when no services are registered.
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// Returns a builder that constructs a registry incrementally.
    ///
    /// ```ignore
    /// let registry = Registry::<MyShard>::builder()
    ///     .timeout(Duration::from_secs(2))
    ///     .service("echo", echo_addr)
    ///     .service("billing", billing_addr)
    ///     .build();
    /// ```
    pub fn builder() -> RegistryBuilder<S> {
        RegistryBuilder::default()
    }
}

/// Incremental builder for [`Registry`].
pub struct RegistryBuilder<S>
where
    S: tina::Shard,
{
    services: HashMap<String, Address<ServiceCall, ServiceReply>>,
    config: RegistryConfig,
    _shard: std::marker::PhantomData<S>,
}

impl<S> std::fmt::Debug for RegistryBuilder<S>
where
    S: tina::Shard,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryBuilder")
            .field("services", &self.services.keys().collect::<Vec<_>>())
            .field("config", &self.config)
            .finish()
    }
}

impl<S> Default for RegistryBuilder<S>
where
    S: tina::Shard,
{
    fn default() -> Self {
        Self {
            services: HashMap::new(),
            config: RegistryConfig::default(),
            _shard: std::marker::PhantomData,
        }
    }
}

impl<S> RegistryBuilder<S>
where
    S: tina::Shard,
{
    /// Registers `addr` as the service named `name`. Re-registering the
    /// same name overwrites the previous binding.
    pub fn service(
        mut self,
        name: impl Into<String>,
        addr: Address<ServiceCall, ServiceReply>,
    ) -> Self {
        self.services.insert(name.into(), addr);
        self
    }

    /// Sets the per-service-call timeout the registry will use on every
    /// `IsolateCall` to a registered service.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.service_call_timeout = timeout;
        self
    }

    /// Replaces the entire configuration.
    pub fn config(mut self, config: RegistryConfig) -> Self {
        self.config = config;
        self
    }

    /// Finalizes the builder.
    pub fn build(self) -> Registry<S> {
        debug_assert!(
            !self.config.service_call_timeout.is_zero(),
            "RegistryConfig::service_call_timeout must be non-zero",
        );
        Registry {
            services: self.services,
            config: self.config,
            _shard: std::marker::PhantomData,
        }
    }
}

impl<S> Registry<S>
where
    S: tina::Shard,
    Self: tina::Isolate<Message = RegistryMsg, Reply = RouterReply, Io = RuntimeCall<RegistryMsg>>,
{
    /// Handles a `Route` call: look up the service, then either answer the
    /// caller now (unknown service) or defer the answer through the downstream
    /// service `IsolateCall`, carrying the caller's [`RequestContext`] into the
    /// [`RegistryMsg::ServiceResult`] continuation.
    fn route(&mut self, request: RouterRequest, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
        let RouterRequest {
            request_id: _,
            service,
            method,
            payload,
        } = request;
        let Some(service_addr) = self.services.get(&service).copied() else {
            return call_ctx.reply(RouterReply::UnknownService);
        };
        call_ctx
            .defer(call(
                service_addr,
                ServiceCall { method, payload },
                self.config.service_call_timeout,
            ))
            .reply(RegistryMsg::ServiceResult)
    }

    /// Maps a downstream service outcome to a [`RouterReply`] and answers the
    /// original caller through its captured [`RequestContext`].
    fn finish(
        &mut self,
        req: RequestContext<RouterReply>,
        outcome: CallOutcome<ServiceReply>,
    ) -> Effect<Self> {
        reply_to(req, outcome_to_router_reply(outcome))
    }
}

/// Maps a downstream service `IsolateCall` outcome to the wire-facing
/// [`RouterReply`]. Pure so the mapping table is unit-testable without a
/// runtime; see the module tests.
fn outcome_to_router_reply(outcome: CallOutcome<ServiceReply>) -> RouterReply {
    match outcome {
        CallOutcome::Replied(ServiceReply::Ok(bytes)) => RouterReply::Ok(bytes),
        CallOutcome::Replied(ServiceReply::UnknownMethod) => RouterReply::UnknownMethod,
        CallOutcome::Replied(ServiceReply::Decode) => RouterReply::Decode,
        CallOutcome::Replied(ServiceReply::Internal) => RouterReply::Internal,
        CallOutcome::Full => RouterReply::Full,
        CallOutcome::Closed => RouterReply::Internal,
        CallOutcome::Rejected(_) => RouterReply::Internal,
        // Wire-error invariant: server-side service timeout maps to
        // Internal on the wire, not Timeout. Timeout is a
        // client-observed condition only.
        CallOutcome::Timeout => RouterReply::Internal,
    }
}

impl<S> tina::Isolate for Registry<S>
where
    S: tina::Shard,
{
    type Message = RegistryMsg;
    type Reply = RouterReply;
    type Send = Outbound<std::convert::Infallible>;
    type Spawn = std::convert::Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Io = RuntimeCall<RegistryMsg>;
    type Fact = ::std::convert::Infallible;
    type Shard = S;

    // Connections reach the registry with `call()`, so the caller authority
    // lives here. `Route` is the only externally-issued variant.
    fn handle_call(&mut self, msg: RegistryMsg, call: CallContext<'_, Self>) -> Effect<Self> {
        match msg {
            RegistryMsg::Route(request) => self.route(request, call),
            // The continuation is a self-delivered send, never a call. A caller
            // that sends `ServiceResult` as a call gets a clean rejection.
            RegistryMsg::ServiceResult(..) => call.reject(CallRejectedReason::UnsupportedMessage),
        }
    }

    fn handle(&mut self, msg: RegistryMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            // A plain send of `Route` carries no caller authority, so there is
            // nothing to answer. Connections always `call()`.
            RegistryMsg::Route(_) => noop(),
            RegistryMsg::ServiceResult(req, outcome) => self.finish(req, outcome),
        }
    }
}

// `handle_call` is the intentional callee surface for connection traffic.
impl<S> CallableIsolate for Registry<S> where S: tina::Shard {}

#[cfg(test)]
mod tests {
    use super::*;

    use tina::{IsolateId, ShardId};

    #[derive(Debug)]
    struct TestShard;

    impl tina::Shard for TestShard {
        fn id(&self) -> ShardId {
            ShardId::new(0)
        }
    }

    fn service_addr(id: u64) -> Address<ServiceCall, ServiceReply> {
        Address::<ServiceCall>::new(ShardId::new(0), IsolateId::new(id))
            .with_reply::<ServiceReply>()
    }

    // The `Route → reply` and deferred `ServiceResult → reply_to` paths run
    // through the runtime `call()` machinery (caller authority cannot be
    // hand-built here), so they are exercised end-to-end in
    // `tests/rpc_end_to_end.rs`. These unit tests cover the pure mapping table
    // and the service-lookup bookkeeping.

    #[test]
    fn service_ok_maps_to_router_reply_ok() {
        let mapped =
            outcome_to_router_reply(CallOutcome::Replied(ServiceReply::Ok(b"hi".to_vec())));
        assert_eq!(mapped, RouterReply::Ok(b"hi".to_vec()));
    }

    #[test]
    fn service_unknown_method_propagates() {
        let mapped = outcome_to_router_reply(CallOutcome::Replied(ServiceReply::UnknownMethod));
        assert_eq!(mapped, RouterReply::UnknownMethod);
    }

    #[test]
    fn service_decode_propagates() {
        let mapped = outcome_to_router_reply(CallOutcome::Replied(ServiceReply::Decode));
        assert_eq!(mapped, RouterReply::Decode);
    }

    #[test]
    fn service_internal_propagates() {
        let mapped = outcome_to_router_reply(CallOutcome::Replied(ServiceReply::Internal));
        assert_eq!(mapped, RouterReply::Internal);
    }

    #[test]
    fn service_full_maps_to_router_reply_full() {
        assert_eq!(
            outcome_to_router_reply(CallOutcome::Full),
            RouterReply::Full
        );
    }

    #[test]
    fn service_closed_maps_to_internal() {
        assert_eq!(
            outcome_to_router_reply(CallOutcome::Closed),
            RouterReply::Internal
        );
    }

    #[test]
    fn service_timeout_maps_to_internal_not_wire_timeout() {
        assert_eq!(
            outcome_to_router_reply(CallOutcome::Timeout),
            RouterReply::Internal
        );
    }

    #[test]
    fn service_rejected_maps_to_internal() {
        assert_eq!(
            outcome_to_router_reply(CallOutcome::Rejected(
                tina::CallRejectedReason::UnsupportedMessage
            )),
            RouterReply::Internal
        );
    }

    #[test]
    fn lookup_reports_registered_service() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        assert!(registry.is_empty());
        registry.register("svc", service_addr(7));
        assert_eq!(registry.len(), 1);
        assert!(registry.services.contains_key("svc"));
        assert!(!registry.services.contains_key("missing"));
    }

    #[test]
    fn deregister_removes_service() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        registry.register("svc", service_addr(1));
        assert_eq!(registry.len(), 1);
        let removed = registry.deregister("svc");
        assert!(removed.is_some());
        assert!(registry.is_empty());
        assert!(!registry.services.contains_key("svc"));
    }

    #[test]
    fn re_register_overwrites() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        registry.register("svc", service_addr(1));
        registry.register("svc", service_addr(2));
        assert_eq!(registry.len(), 1);
    }
}
