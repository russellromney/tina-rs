//! Service topology adapters (phase 058 Rock 1).
//!
//! 052 left a deliberate question open: a "service" in `tina-rpc` is
//! whatever isolate the [`crate::Registry`] hands traffic to via an
//! `Address<ServiceCall, ServiceReply>`, but is that isolate
//!
//! - one process-wide instance,
//! - a bounded pool of N workers, or
//! - a sharded set keyed by some part of the request?
//!
//! 058 picks the answer before [`crate::Registry`] grows users that
//! depend on the wrong one. This module sketches the three supported
//! shapes and ships the first-form ([`SingleService`]) implementation.
//! [`PooledService`] and [`ShardedService`] are deliberately
//! unimplemented; their type signatures exist as a *commitment* —
//! later phases fill in the bodies, not the surface.
//!
//! # The three shapes
//!
//! ## Single ([`SingleService`])
//!
//! - **Topology:** one [`ServiceHandler`] in one Tina isolate.
//! - **Capacity:** one `mailbox_capacity` setting on
//!   [`ServiceConfig`]. Pressure shows up as
//!   `CallOutcome::Full` at the registry's `IsolateCall`, mapped to
//!   `RouterReply::Full`, mapped to wire `Error(Full)` —
//!   the same path 052 already proves. No new pressure surface.
//! - **When to use:** stateful services that need linearizable access
//!   to the same `&mut self`. Default for first form.
//!
//! ## Pooled ([`PooledService`], not yet implemented)
//!
//! - **Topology:** N identical worker isolates behind a tiny
//!   round-robin frontend isolate. The frontend's address is what the
//!   registry stores; the workers are children of the frontend.
//! - **Capacity:** the frontend's mailbox is the registry-visible
//!   queue. Each worker has its own mailbox. Pressure can show up
//!   at *either* layer; first-form rule will be: the frontend's
//!   mailbox is `mailbox_capacity * pool_size`, and overflow there
//!   is the only `Full` users observe (workers internally route via
//!   the frontend's stable backpressure).
//! - **When to use:** stateless services where horizontal capacity
//!   helps. The frontend keeps the registry blissfully unaware that
//!   "service" actually means N isolates.
//!
//! ## Sharded ([`ShardedService`], not yet implemented)
//!
//! - **Topology:** keyed routing across N shard isolates, one shard
//!   owning a slice of state space. Reuses 053's sharded primitives
//!   when those land.
//! - **Capacity:** per-shard mailbox; pressure on a hot shard is a
//!   hot-shard `Full`, not a global `Full`. (This is the load
//!   isolation a sharded design buys you.)
//! - **When to use:** stateful services where state can be split by
//!   key (per-user, per-tenant, etc.).
//!
//! # The registry stays unchanged
//!
//! All three shapes produce a single
//! `Address<ServiceCall, ServiceReply>` that
//! [`crate::Registry::register`] consumes the same way. The
//! pool/shard fan-out lives *inside* the address recipient — a tiny
//! frontend isolate that owns the workers/shards. The registry never
//! grows a "topology" enum, never has to learn how to round-robin or
//! hash. Topology is an adapter concern.
//!
//! ```text
//!   Connection isolate ─call──▶ Registry ─call──▶ Address (Service)
//!                                                  │
//!                            ┌─────────────────────┴───────┐
//!                            │           │                 │
//!                       SingleService  PooledService   ShardedService
//!                       (1 worker)     (N workers       (N shards
//!                                       round-robin)     by key)
//! ```
//!
//! ## Per-hop timeout budget (a real cost, not a free lunch)
//!
//! "Registry stays unchanged" is true for the *type* — the
//! registry's `IsolateCall` to its registered address still has the
//! same signature. The cost the registry can't see: pool and
//! sharded adapters add a *second* `IsolateCall` hop on the server
//! (registry → frontend → worker). Today's
//! `RegistryConfig::service_call_timeout` (default 4 s) bounds *one*
//! hop. With a pool, that 4 s budget has to cover the frontend's
//! enqueue + the worker's enqueue + the worker's dispatch + the
//! return chain.
//!
//! The implementation phase for [`PooledService`] /
//! [`ShardedService`] will need to either:
//!
//! - forward the registry-side deadline down to the frontend's own
//!   `IsolateCall` to a worker (so the chain shares one budget); or
//! - set the frontend's per-worker timeout to a documented fraction
//!   of the registry timeout, accepting that some pool calls will
//!   look like `Internal` to the client when the worker takes too
//!   long even though the client's deadline hadn't elapsed.
//!
//! Rock 1 doesn't pick — it locks in the surface where the choice
//! lives. The registry's contract stays "one hop, bounded by my
//! configured timeout"; topology adapters that *internally* fan out
//! own their own per-internal-hop budget.
//!
//! # Convenience-limit rule
//!
//! Per 058's convenience-limit section: every adapter's mailbox
//! capacity is inspectable on [`ServiceConfig`]. A future
//! `#[tina_rpc::service]` macro must thread that config through, not
//! pick its own hidden default.

use std::convert::Infallible;
use std::marker::PhantomData;

use tina::prelude::*;
use tina_runtime::RuntimeCall;

use crate::registry::{ServiceCall, ServiceReply};

/// Synchronous request-to-reply dispatch.
///
/// What the (future) `#[tina_rpc::service]` macro generates and what
/// application code can implement directly today. The
/// [`SingleService`] adapter wraps a `ServiceHandler` into a Tina
/// isolate; [`PooledService`] and [`ShardedService`] (later) reuse
/// the same `ServiceHandler` trait so the macro output is
/// topology-agnostic.
///
/// # Why sync
///
/// First form: `dispatch` returns a [`ServiceReply`] synchronously.
/// Handlers that need to do their own runtime calls (sleep, file
/// I/O, downstream RPC) belong in a later phase — that's a
/// substantially different shape because each handler turn would
/// need access to Tina's effect machinery. Keeping the first form
/// sync lets the macro stay a pure data transform: decode bytes,
/// call user method, encode bytes.
///
/// # Error mapping
///
/// Handlers report typed errors by returning
/// [`ServiceReply::UnknownMethod`], [`ServiceReply::Decode`], or
/// [`ServiceReply::Internal`]. Domain errors that round-trip as
/// payload bytes belong on the success path: encode them inside
/// `ServiceReply::Ok(bytes)` and have the client decode. The macro
/// (Rock 3) will codify that convention.
///
/// # The `'static` bound
///
/// `ServiceHandler: 'static` is required because the wrapping
/// adapter is a Tina isolate, and runtime registration needs
/// `I: 'static`. A handler implemented on a struct with no
/// lifetime parameters satisfies this trivially. A handler that
/// wants to borrow from the surrounding stack does not — that's a
/// shape Tina actor systems don't support, and the future
/// `#[tina_rpc::service]` macro emits owned `Self`-only impls.
pub trait ServiceHandler<S>: 'static
where
    S: tina::Shard,
{
    /// Process one call and return one reply synchronously.
    ///
    /// Surfacing a non-domain failure (a panic-equivalent the
    /// service caught at the boundary) is what
    /// [`ServiceReply::Internal`] is for. Returning
    /// [`ServiceReply::Decode`] tells the client the payload was
    /// malformed; [`ServiceReply::UnknownMethod`] tells it the
    /// method name didn't dispatch.
    fn dispatch(&mut self, call: ServiceCall) -> ServiceReply;
}

/// Configuration shared by every topology adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Bounded mailbox capacity for the registry-facing isolate
    /// (whatever it turns out to be: the single instance, the
    /// pool's frontend, or the sharded router). Pressure beyond
    /// this surfaces as `CallOutcome::Full` at the registry's
    /// `IsolateCall`, then as `RouterReply::Full`, then as wire
    /// `Error(Full)`.
    pub mailbox_capacity: usize,
}

impl Default for ServiceConfig {
    /// 64 — small enough to feel pressure under bursty load,
    /// large enough that occasional spikes don't immediately
    /// surface as `Full`.
    fn default() -> Self {
        Self {
            mailbox_capacity: 64,
        }
    }
}

// ---------------------------------------------------------------------------
// Single
// ---------------------------------------------------------------------------

/// First-form adapter: one [`ServiceHandler`] in one Tina isolate.
///
/// Constructed via [`SingleService::new`]; register the resulting
/// `Address<ServiceCall, ServiceReply>` with [`crate::Registry`].
///
/// # Mailbox capacity
///
/// `SingleService` does not carry a [`ServiceConfig`] in Rock 1
/// because the mailbox bound is set at *registration time* by the
/// runtime helper (`runtime.register_with_capacity(adapter, n)`).
/// `ServiceConfig::mailbox_capacity` exists as a documented surface
/// the future `#[tina_rpc::service]` macro and a
/// `register_service` helper will thread through; it is deliberately
/// not collected here so two ways to set one number can't drift.
///
/// # Example
///
/// ```ignore
/// struct Echo;
/// impl tina_rpc::ServiceHandler<MyShard> for Echo {
///     fn dispatch(&mut self, call: tina_rpc::ServiceCall) -> tina_rpc::ServiceReply {
///         tina_rpc::ServiceReply::Ok(call.payload)
///     }
/// }
///
/// let cap = tina_rpc::ServiceConfig::default().mailbox_capacity;
/// let addr = runtime.register_with_capacity::<SingleService<Echo, MyShard>, _>(
///     SingleService::new(Echo),
///     cap,
/// );
/// registry.register("echo", addr);
/// ```
pub struct SingleService<H, S>
where
    H: ServiceHandler<S>,
    S: tina::Shard,
{
    handler: H,
    _shard: PhantomData<S>,
}

impl<H, S> std::fmt::Debug for SingleService<H, S>
where
    H: ServiceHandler<S>,
    S: tina::Shard,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleService")
            .field("handler", &std::any::type_name::<H>())
            .finish()
    }
}

impl<H, S> SingleService<H, S>
where
    H: ServiceHandler<S>,
    S: tina::Shard,
{
    /// Wraps `handler` in a single-isolate adapter. The mailbox
    /// bound is set at registration time, not here; see the type
    /// docs for the rationale.
    pub fn new(handler: H) -> Self {
        Self {
            handler,
            _shard: PhantomData,
        }
    }
}

impl<H, S> tina::Isolate for SingleService<H, S>
where
    H: ServiceHandler<S>,
    S: tina::Shard,
{
    type Message = ServiceCall;
    type Reply = ServiceReply;
    type Send = Outbound<Infallible>;
    type Spawn = Infallible;
    type Call = RuntimeCall<ServiceCall>;
    type Shard = S;

    fn handle(&mut self, msg: ServiceCall, _ctx: &mut Context<'_, S>) -> Effect<Self> {
        reply::<Self>(self.handler.dispatch(msg))
    }
}

// ---------------------------------------------------------------------------
// Pooled (stub)
// ---------------------------------------------------------------------------

/// Bounded service pool. **Not implemented in Rock 1**: the type
/// shape is reserved here so the registry contract and future
/// `#[tina_rpc::service]` macro can name it without locking it in.
///
/// **No public constructor exists.** A user who reaches for
/// `PooledService::new(...)` gets a compile error pointing at
/// missing functionality, not a runtime panic. The type is a
/// commitment, not a footgun.
///
/// ```compile_fail
/// // Building a `PooledService` from outside the module must not
/// // compile. No public `new`, and a private `Sealed` field blocks
/// // struct-literal construction.
/// use std::marker::PhantomData;
/// use tina_rpc::PooledService;
/// # struct EchoHandler;
/// # #[derive(Debug, Default)]
/// # struct TestShard;
/// # impl tina::Shard for TestShard {
/// #     fn id(&self) -> tina::ShardId { tina::ShardId::new(91) }
/// # }
/// # impl tina_rpc::ServiceHandler<TestShard> for EchoHandler {
/// #     fn dispatch(&mut self, _call: tina_rpc::ServiceCall) -> tina_rpc::ServiceReply {
/// #         tina_rpc::ServiceReply::Ok(Vec::new())
/// #     }
/// # }
/// let _ = PooledService::<EchoHandler, TestShard>::new(EchoHandler);
/// ```
///
/// # Planned shape
///
/// - `PooledService<H, S>` is itself a Tina isolate with
///   `Message = ServiceCall, Reply = ServiceReply`.
/// - It owns N child `SingleService<H, S>` workers (or equivalent)
///   spawned through the runtime's supervision API.
/// - `handle` round-robins across the workers via `IsolateCall`,
///   forwarding the deferred-reply call_context the same way
///   [`crate::Registry`] does today (it already chains through one
///   isolate-call hop; pool adds a second).
/// - The implementation phase decides whether `pool_size` and
///   per-worker mailbox capacity live on [`ServiceConfig`]
///   (single source of truth) or on the constructor (per-adapter
///   knobs); Rock 1 deliberately does not commit either way.
///
/// # Pressure semantics
///
/// The frontend is what the registry sees, so the user-observable
/// `Full` happens at the frontend's mailbox cap. Workers have
/// their own per-call backpressure: the frontend's `IsolateCall`
/// to a saturated worker resolves as `CallOutcome::Full`. Whether
/// the frontend re-routes to a different worker or surfaces that
/// as a registry `Full` of its own is a deliberate choice the
/// implementation phase will make.
pub struct PooledService<H, S>
where
    H: ServiceHandler<S>,
    S: tina::Shard,
{
    _handler: PhantomData<H>,
    _shard: PhantomData<S>,
    // Private field with a private type prevents struct-literal
    // construction outside this module. There is no public `new`,
    // so the only way to compile a `PooledService` is to wait for
    // the implementation phase to add one.
    _seal: Sealed,
}

// ---------------------------------------------------------------------------
// Sharded (stub)
// ---------------------------------------------------------------------------

/// Sharded service set keyed by some part of the request. **Not
/// implemented in Rock 1**: depends on phase 053's sharded
/// primitives.
///
/// **No public constructor exists.** Like [`PooledService`], the
/// type is a reservation, not a runtime trap. Reaching for it
/// produces a compile error.
///
/// ```compile_fail
/// use tina_rpc::ShardedService;
/// # struct EchoHandler;
/// # #[derive(Debug, Default)]
/// # struct TestShard;
/// # impl tina::Shard for TestShard {
/// #     fn id(&self) -> tina::ShardId { tina::ShardId::new(91) }
/// # }
/// # impl tina_rpc::ServiceHandler<TestShard> for EchoHandler {
/// #     fn dispatch(&mut self, _call: tina_rpc::ServiceCall) -> tina_rpc::ServiceReply {
/// #         tina_rpc::ServiceReply::Ok(Vec::new())
/// #     }
/// # }
/// let _ = ShardedService::<EchoHandler, TestShard>::new(EchoHandler);
/// ```
///
/// # Planned shape
///
/// - `ShardedService<H, S, K>` is a Tina isolate frontend that
///   inspects each `ServiceCall`, extracts a routing key via a
///   user-supplied `K: Fn(&ServiceCall) -> ShardId`, and
///   `IsolateCall`s the matching shard worker.
/// - Workers are themselves single-handler isolates, one per shard.
/// - State is partitioned by shard; a hot-shard request load
///   surfaces as a per-shard `Full`, not a global one. This is
///   the load-isolation argument for sharding.
///
/// # Why this depends on 053
///
/// Phase 053 ("sharded service primitives") is supposed to land
/// shard-id types, key extractors, and the runtime support for
/// directing `IsolateCall`s by shard. Trying to ship `ShardedService`
/// before those exist would either reinvent that surface or ship a
/// half-shape that doesn't compose. The stub here exists so the
/// `#[service]` macro can grow `#[service(shard = ...)]` syntax
/// without locking in an implementation today.
pub struct ShardedService<H, S>
where
    H: ServiceHandler<S>,
    S: tina::Shard,
{
    _handler: PhantomData<H>,
    _shard: PhantomData<S>,
    _seal: Sealed,
}

/// Crate-private "sealed" marker that prevents struct-literal
/// construction of [`PooledService`] / [`ShardedService`] from
/// outside this module. Combined with the absence of a public
/// `new`, this turns "topology not yet implemented" into a compile
/// error rather than a runtime panic.
struct Sealed;

#[cfg(test)]
mod tests {
    use super::*;

    use tina::{IsolateId, ShardId};

    #[derive(Debug, Default)]
    struct TestShard;

    impl tina::Shard for TestShard {
        fn id(&self) -> ShardId {
            ShardId::new(91)
        }
    }

    /// Echoes every request payload back as `Ok`. Demonstrates the
    /// "trivial" shape: one method, no decode, no method dispatch.
    struct EchoHandler;

    impl ServiceHandler<TestShard> for EchoHandler {
        fn dispatch(&mut self, call: ServiceCall) -> ServiceReply {
            ServiceReply::Ok(call.payload)
        }
    }

    /// Routes by method name to demonstrate UnknownMethod / Decode
    /// / Internal mappings without any macro magic.
    struct ManualDispatch;

    impl ServiceHandler<TestShard> for ManualDispatch {
        fn dispatch(&mut self, call: ServiceCall) -> ServiceReply {
            match call.method.as_str() {
                "echo" => ServiceReply::Ok(call.payload),
                "bad_input" => ServiceReply::Decode,
                "boom" => ServiceReply::Internal,
                _ => ServiceReply::UnknownMethod,
            }
        }
    }

    fn dispatch_to_isolate<H>(
        adapter: &mut SingleService<H, TestShard>,
        msg: ServiceCall,
    ) -> Effect<SingleService<H, TestShard>>
    where
        H: ServiceHandler<TestShard>,
    {
        let mut shard = TestShard;
        let mut ctx = Context::new(&mut shard, IsolateId::new(99));
        adapter.handle(msg, &mut ctx)
    }

    fn extract_reply<H>(effect: Effect<SingleService<H, TestShard>>) -> ServiceReply
    where
        H: ServiceHandler<TestShard>,
    {
        match effect {
            Effect::Reply(reply) => reply,
            other => panic!("expected Effect::Reply, got {other:?}"),
        }
    }

    #[test]
    fn single_service_echoes_payload() {
        let mut svc = SingleService::new(EchoHandler);
        let effect = dispatch_to_isolate(
            &mut svc,
            ServiceCall {
                method: "echo".into(),
                payload: b"hi".to_vec(),
            },
        );
        assert_eq!(extract_reply(effect), ServiceReply::Ok(b"hi".to_vec()));
    }

    #[test]
    fn single_service_propagates_unknown_method() {
        let mut svc = SingleService::new(ManualDispatch);
        let effect = dispatch_to_isolate(
            &mut svc,
            ServiceCall {
                method: "no_such_method".into(),
                payload: Vec::new(),
            },
        );
        assert_eq!(extract_reply(effect), ServiceReply::UnknownMethod);
    }

    #[test]
    fn single_service_propagates_decode() {
        let mut svc = SingleService::new(ManualDispatch);
        let effect = dispatch_to_isolate(
            &mut svc,
            ServiceCall {
                method: "bad_input".into(),
                payload: vec![0xff],
            },
        );
        assert_eq!(extract_reply(effect), ServiceReply::Decode);
    }

    #[test]
    fn single_service_propagates_internal() {
        let mut svc = SingleService::new(ManualDispatch);
        let effect = dispatch_to_isolate(
            &mut svc,
            ServiceCall {
                method: "boom".into(),
                payload: Vec::new(),
            },
        );
        assert_eq!(extract_reply(effect), ServiceReply::Internal);
    }

    #[test]
    fn handler_state_persists_across_calls() {
        // Counter handler proves `&mut self` is real — a stateful
        // service that mutates between calls works the same way it
        // would in any other Tina isolate.
        struct Counter {
            count: u64,
        }

        impl ServiceHandler<TestShard> for Counter {
            fn dispatch(&mut self, _call: ServiceCall) -> ServiceReply {
                self.count += 1;
                ServiceReply::Ok(self.count.to_be_bytes().to_vec())
            }
        }

        let mut svc = SingleService::new(Counter { count: 0 });
        let one = dispatch_to_isolate(
            &mut svc,
            ServiceCall {
                method: "tick".into(),
                payload: Vec::new(),
            },
        );
        let two = dispatch_to_isolate(
            &mut svc,
            ServiceCall {
                method: "tick".into(),
                payload: Vec::new(),
            },
        );
        assert_eq!(
            extract_reply(one),
            ServiceReply::Ok(1u64.to_be_bytes().to_vec())
        );
        assert_eq!(
            extract_reply(two),
            ServiceReply::Ok(2u64.to_be_bytes().to_vec())
        );
    }

    #[test]
    fn single_service_emits_only_reply_no_call_or_send() {
        // Lock the contract: SingleService must return Effect::Reply
        // and never an Effect::Call (which would mean a runtime hop)
        // or Effect::Send (which would route bytes elsewhere). A
        // future change that makes handlers async-capable should
        // change this test deliberately, not silently.
        let mut svc = SingleService::new(EchoHandler);
        let effect = dispatch_to_isolate(
            &mut svc,
            ServiceCall {
                method: "echo".into(),
                payload: Vec::new(),
            },
        );
        match effect {
            Effect::Reply(_) => {}
            other => panic!("expected Effect::Reply, got {other:?}"),
        }
    }
}
