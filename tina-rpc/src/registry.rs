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
//! external `RouterRequest`s and *receives* internal continuation messages
//! (the service-call results). They share the registry's mailbox, so its
//! message vocabulary is an envelope:
//!
//! - `RegistryMsg::Route(RouterRequest)` — external entrypoint from a
//!   connection isolate.
//! - `RegistryMsg::ServiceResult(CallOutcome<ServiceReply>)` — internal
//!   continuation, the translator output for a service `IsolateCall`.
//!
//! The connection sends only `Route(...)`. `ServiceResult` is an
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
/// hijack a victim's pending reply: the runtime pairs replies with the
/// per-call `call_context`, not with anything in the message. A spoofed
/// `ServiceResult` either rides a fresh `IsolateCall` (in which case the
/// reply lands at the spoofer's own pending slot) or rides a `send` (in
/// which case the runtime logs the stray `Effect::Reply` and drops it).
/// Misbehavior is contained to the misbehaving isolate.
#[derive(Debug, Clone)]
pub enum RegistryMsg {
    /// External request from a connection isolate.
    Route(RouterRequest),
    /// Internal continuation: a service `IsolateCall` completed.
    ///
    /// The variant carries only the outcome; the request id and service
    /// name needed to re-form a reply are already implied by the runtime's
    /// `call_context` machinery, so storing them on the wire-side
    /// continuation message would be redundant and would expose extra
    /// fields a hostile in-process actor could attempt to manipulate.
    #[doc(hidden)]
    ServiceResult(CallOutcome<ServiceReply>),
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
    Self:
        tina::Isolate<Message = RegistryMsg, Reply = RouterReply, Call = RuntimeCall<RegistryMsg>>,
{
    fn route(&mut self, request: RouterRequest) -> Effect<Self> {
        let RouterRequest {
            request_id: _,
            service,
            method,
            payload,
        } = request;
        let Some(service_addr) = self.services.get(&service).copied() else {
            return reply::<Self>(RouterReply::UnknownService);
        };
        call(
            service_addr,
            ServiceCall { method, payload },
            self.config.service_call_timeout,
        )
        .reply(RegistryMsg::ServiceResult)
    }

    fn finish(&mut self, outcome: CallOutcome<ServiceReply>) -> Effect<Self> {
        let mapped = match outcome {
            CallOutcome::Replied(ServiceReply::Ok(bytes)) => RouterReply::Ok(bytes),
            CallOutcome::Replied(ServiceReply::UnknownMethod) => RouterReply::UnknownMethod,
            CallOutcome::Replied(ServiceReply::Decode) => RouterReply::Decode,
            CallOutcome::Replied(ServiceReply::Internal) => RouterReply::Internal,
            CallOutcome::Full => RouterReply::Full,
            CallOutcome::Closed => RouterReply::Internal,
            // Wire-error invariant: server-side service timeout maps to
            // Internal on the wire, not Timeout. Timeout is a
            // client-observed condition only.
            CallOutcome::Timeout => RouterReply::Internal,
        };
        reply::<Self>(mapped)
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
    type Call = RuntimeCall<RegistryMsg>;
    type Shard = S;

    fn handle(&mut self, msg: RegistryMsg, _ctx: &mut Context<'_, S, Self::Reply>) -> Effect<Self> {
        match msg {
            RegistryMsg::Route(request) => self.route(request),
            RegistryMsg::ServiceResult(outcome) => self.finish(outcome),
        }
    }
}

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

    fn dispatch(
        registry: &mut Registry<TestShard>,
        msg: RegistryMsg,
    ) -> Effect<Registry<TestShard>> {
        let mut shard = TestShard;
        let mut ctx = Context::<_, RouterReply>::new_typed(&mut shard, IsolateId::new(99));
        registry.handle(msg, &mut ctx)
    }

    fn make_request(service: &str, method: &str) -> RouterRequest {
        RouterRequest {
            request_id: 1,
            service: service.into(),
            method: method.into(),
            payload: Vec::new(),
        }
    }

    #[test]
    fn unknown_service_replies_unknown_service() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        let effect = dispatch(
            &mut registry,
            RegistryMsg::Route(make_request("missing", "m")),
        );
        match effect {
            Effect::Reply(RouterReply::UnknownService) => {}
            other => panic!("expected Reply(UnknownService), got {other:?}"),
        }
    }

    #[test]
    fn known_service_emits_isolate_call_continuation() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        registry.register("svc", service_addr(7));
        let effect = dispatch(&mut registry, RegistryMsg::Route(make_request("svc", "m")));
        // Should be Effect::Call (an isolate-call to the service).
        assert!(matches!(effect, Effect::Call(_)));
    }

    #[test]
    fn service_ok_maps_to_router_reply_ok() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        registry.register("svc", service_addr(1));
        let effect = dispatch(
            &mut registry,
            RegistryMsg::ServiceResult(CallOutcome::Replied(ServiceReply::Ok(b"hi".to_vec()))),
        );
        match effect {
            Effect::Reply(RouterReply::Ok(bytes)) => assert_eq!(bytes, b"hi"),
            other => panic!("expected Reply(Ok), got {other:?}"),
        }
    }

    #[test]
    fn service_unknown_method_propagates() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        let effect = dispatch(
            &mut registry,
            RegistryMsg::ServiceResult(CallOutcome::Replied(ServiceReply::UnknownMethod)),
        );
        assert!(matches!(effect, Effect::Reply(RouterReply::UnknownMethod)));
    }

    #[test]
    fn service_decode_propagates() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        let effect = dispatch(
            &mut registry,
            RegistryMsg::ServiceResult(CallOutcome::Replied(ServiceReply::Decode)),
        );
        assert!(matches!(effect, Effect::Reply(RouterReply::Decode)));
    }

    #[test]
    fn service_internal_propagates() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        let effect = dispatch(
            &mut registry,
            RegistryMsg::ServiceResult(CallOutcome::Replied(ServiceReply::Internal)),
        );
        assert!(matches!(effect, Effect::Reply(RouterReply::Internal)));
    }

    #[test]
    fn service_full_maps_to_router_reply_full() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        let effect = dispatch(&mut registry, RegistryMsg::ServiceResult(CallOutcome::Full));
        assert!(matches!(effect, Effect::Reply(RouterReply::Full)));
    }

    #[test]
    fn service_closed_maps_to_internal() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        let effect = dispatch(
            &mut registry,
            RegistryMsg::ServiceResult(CallOutcome::Closed),
        );
        assert!(matches!(effect, Effect::Reply(RouterReply::Internal)));
    }

    #[test]
    fn service_timeout_maps_to_internal_not_wire_timeout() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        let effect = dispatch(
            &mut registry,
            RegistryMsg::ServiceResult(CallOutcome::Timeout),
        );
        assert!(matches!(effect, Effect::Reply(RouterReply::Internal)));
    }

    #[test]
    fn deregister_removes_service() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        registry.register("svc", service_addr(1));
        assert_eq!(registry.len(), 1);
        let removed = registry.deregister("svc");
        assert!(removed.is_some());
        assert!(registry.is_empty());
        // Re-route should now report unknown service.
        let effect = dispatch(&mut registry, RegistryMsg::Route(make_request("svc", "m")));
        assert!(matches!(effect, Effect::Reply(RouterReply::UnknownService)));
    }

    #[test]
    fn re_register_overwrites() {
        let mut registry = Registry::<TestShard>::new(RegistryConfig::default());
        registry.register("svc", service_addr(1));
        registry.register("svc", service_addr(2));
        assert_eq!(registry.len(), 1);
    }
}
