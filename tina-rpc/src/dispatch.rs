//! Typed service dispatch core.
//!
//! Sits one level below the topology adapters: turns a `ServiceCall`
//! (method name + opaque bytes) into a typed method invocation with
//! size-capped decode and encode. Runtime-free.
//!
//! ```text
//!   ServiceCall { method, payload }
//!         │
//!         ▼  MethodTable::dispatch — lookup by name
//!   Method::invoke
//!         │  decode bytes → typed Req
//!         │  call user handler with &mut State
//!         │  encode typed Resp → bytes
//!         ▼
//!   ServiceReply::{Ok | UnknownMethod | Decode | Internal}
//! ```
//!
//! `#[tina_rpc::service]` is mostly syntax over [`MethodTable`] /
//! [`Method`] / [`Dispatch`].
//!
//! # Wire-error mapping
//!
//! - Unknown method → [`ServiceReply::UnknownMethod`] → wire
//!   `Error(UnknownMethod)`.
//! - Request decode fails (including oversize past
//!   `limits.max_request`) → [`ServiceReply::Decode`] → wire
//!   `Error(Decode)`.
//! - Reply encode fails (oversize past `limits.max_response`,
//!   encoder refuses, or handler panic caught at the boundary) →
//!   [`ServiceReply::Internal`] → wire `Error(Internal)`.
//! - Success → [`ServiceReply::Ok(bytes)`] → wire `Reply(bytes)`.
//!
//! Domain `Err(..)` from a handler `Result<T, E>` rides the success
//! path: the encoder serializes both arms, the bytes go in
//! `ServiceReply::Ok`, the client decodes the `Result`. The wire
//! error codes are reserved for conditions the client can't see
//! from its side.
//!
//! # Size caps
//!
//! [`Method`] runs through [`Encoding::decode`] and
//! [`Encoding::encode`] under a [`PayloadLimits`] budget — request
//! and response sides bound independently.
//! [`PayloadLimits::symmetric`] when shapes are roughly the same.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::encoding::Encoding;
use crate::registry::{ServiceCall, ServiceReply};
use crate::service::ServiceHandler;

/// Per-call payload size budget, separately bounded for request
/// and response.
///
/// The two caps are independent so an asymmetric service (small
/// request → large response, or vice versa) doesn't have to size
/// both around the larger side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadLimits {
    /// Maximum bytes of inbound request payload. Enforced before
    /// the underlying deserializer runs (see [`Encoding::decode`]).
    pub max_request: usize,
    /// Maximum bytes of outbound response payload. Enforced
    /// during serialization (see [`Encoding::encode`]).
    pub max_response: usize,
}

impl PayloadLimits {
    /// Same cap on both sides — convenient when the request and
    /// response shapes are roughly symmetric.
    pub const fn symmetric(bytes: usize) -> Self {
        Self {
            max_request: bytes,
            max_response: bytes,
        }
    }
}

impl Default for PayloadLimits {
    /// 64 KiB on both sides. Comfortable for typical JSON payloads;
    /// stricter than the connection's default `max_frame_size` so
    /// the wire envelope is the looser constraint.
    fn default() -> Self {
        Self::symmetric(64 * 1024)
    }
}

/// The internal shape of a typed method's boxed handler closure.
///
/// `Send` is required so a `Dispatch` containing the closure can be
/// registered with `ThreadedRuntime::register_with_capacity`, which
/// moves isolates across threads.
type InvokeFn<St, E> = Box<dyn Fn(&mut St, &[u8], &E, &PayloadLimits) -> ServiceReply + Send>;

/// One typed method entry: a method name plus a boxed closure that
/// knows how to decode the request bytes, call the user handler with
/// `&mut State`, and encode the reply.
///
/// Construct via [`Method::new`].
pub struct Method<St, E>
where
    E: Encoding,
{
    name: &'static str,
    invoke: InvokeFn<St, E>,
}

impl<St, E> std::fmt::Debug for Method<St, E>
where
    E: Encoding,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Method").field("name", &self.name).finish()
    }
}

impl<St, E> Method<St, E>
where
    E: Encoding,
{
    /// Builds a typed method.
    ///
    /// `Req` and `Resp` are the user's typed request and response;
    /// the closure runs after decode and before encode. The wire
    /// mapping is:
    ///
    /// - `encoding.decode::<Req>(payload, limits.max_request)`
    ///   produces `Req` or [`ServiceReply::Decode`].
    /// - `f(state, req)` runs synchronously and returns `Resp`.
    /// - `encoding.encode(&resp, limits.max_response)` produces
    ///   bytes or [`ServiceReply::Internal`].
    ///
    /// # `Fn`, not `FnMut`
    ///
    /// The handler is `Fn` (not `FnMut`) on purpose — handler-private
    /// mutation belongs on `&mut St`, not in closure captures. A
    /// counter or cache lives on `St` and is mutated through the
    /// `state` argument; the closure itself is a pure dispatch
    /// shim. This also lets the boxed handler be invoked many
    /// times against the same method-table entry.
    ///
    /// # Domain errors ride the success path
    ///
    /// Methods that can fail domain-side return
    /// `Result<Ok, DomainErr>` directly; the encoding handles both
    /// arms and the bytes ride [`ServiceReply::Ok`]. Reserve
    /// [`ServiceReply::Internal`] for transport-level capability
    /// faults — the dispatch core uses it for "encoding refused
    /// the response" and for caught panics; the wire code
    /// `Internal` is intentionally coarse for callers who can't
    /// tell the difference between "your request hit a bug" and
    /// "the encoder couldn't represent the value."
    ///
    /// # Panic safety
    ///
    /// A panic inside `f` is caught at the dispatch boundary and
    /// surfaces as [`ServiceReply::Internal`]; the service isolate
    /// stays alive. `&mut St` is treated as `AssertUnwindSafe`,
    /// which means a panic mid-mutation can leave the state in an
    /// inconsistent intermediate form. Handlers that mutate state
    /// then panic should clean up explicitly (or, simpler, avoid
    /// panicking on caller-supplied input — return a domain
    /// `Err(...)` instead).
    pub fn new<Req, Resp, F>(name: &'static str, f: F) -> Self
    where
        Req: DeserializeOwned + 'static,
        Resp: Serialize + 'static,
        F: Fn(&mut St, Req) -> Resp + Send + 'static,
    {
        let invoke = move |state: &mut St,
                           payload: &[u8],
                           encoding: &E,
                           limits: &PayloadLimits|
              -> ServiceReply {
            let req: Req = match encoding.decode(payload, limits.max_request) {
                Ok(req) => req,
                Err(_) => return ServiceReply::Decode,
            };
            // Catch handler panics and surface them as Internal so
            // a single bad request doesn't tear the service isolate
            // down. State is treated as AssertUnwindSafe — see the
            // "Panic safety" doc above.
            let resp = match catch_unwind(AssertUnwindSafe(|| f(state, req))) {
                Ok(resp) => resp,
                Err(_) => return ServiceReply::Internal,
            };
            match encoding.encode(&resp, limits.max_response) {
                Ok(bytes) => ServiceReply::Ok(bytes),
                Err(_) => ServiceReply::Internal,
            }
        };
        Self {
            name,
            invoke: Box::new(invoke),
        }
    }

    /// Returns the method name this entry dispatches.
    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// Method-name → typed handler lookup.
///
/// Build with [`MethodTable::new`] and [`MethodTable::method`]
/// (chainable). The future `#[tina_rpc::service]` macro emits a
/// `MethodTable` of one [`Method`] per trait method.
pub struct MethodTable<St, E>
where
    E: Encoding,
{
    methods: HashMap<&'static str, Method<St, E>>,
}

impl<St, E> std::fmt::Debug for MethodTable<St, E>
where
    E: Encoding,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MethodTable")
            .field("methods", &self.methods.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl<St, E> Default for MethodTable<St, E>
where
    E: Encoding,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<St, E> MethodTable<St, E>
where
    E: Encoding,
{
    /// Builds an empty method table.
    pub fn new() -> Self {
        Self {
            methods: HashMap::new(),
        }
    }

    /// Adds a method.
    ///
    /// **Panics** if a method with the same `name` was already
    /// added. The `#[service]` macro emits each trait method
    /// exactly once, and hand-written tables should too — a
    /// duplicate is always a programming bug, not a hot-reload
    /// pattern. Failing loudly here means the macro author and any
    /// hand-rolled-table user see the conflict immediately rather
    /// than silently dropping one impl.
    pub fn method(mut self, method: Method<St, E>) -> Self {
        let name = method.name;
        let previous = self.methods.insert(name, method);
        assert!(
            previous.is_none(),
            "duplicate method name '{name}' in MethodTable; \
             every method name must be unique",
        );
        self
    }

    /// Returns the number of registered methods.
    pub fn len(&self) -> usize {
        self.methods.len()
    }

    /// True when no methods are registered.
    pub fn is_empty(&self) -> bool {
        self.methods.is_empty()
    }

    /// Dispatches one [`ServiceCall`].
    ///
    /// `limits.max_request` bounds the request decode; the
    /// underlying [`Encoding`] rejects before deserializing.
    /// `limits.max_response` bounds the reply encode; the encoder
    /// caps during serialization.
    pub fn dispatch(
        &self,
        state: &mut St,
        call: ServiceCall,
        encoding: &E,
        limits: &PayloadLimits,
    ) -> ServiceReply {
        match self.methods.get(call.method.as_str()) {
            Some(method) => (method.invoke)(state, &call.payload, encoding, limits),
            None => ServiceReply::UnknownMethod,
        }
    }
}

/// A [`ServiceHandler`] backed by a typed [`MethodTable`].
///
/// Owns the user state, the method table, the encoding, and the
/// per-call payload budget. Constructed via [`Dispatch::new`] and
/// passed to one of the topology adapters
/// ([`crate::SingleService`] etc.) for runtime registration.
pub struct Dispatch<St, E, Sh>
where
    E: Encoding,
    Sh: tina::Shard,
{
    state: St,
    table: MethodTable<St, E>,
    encoding: E,
    limits: PayloadLimits,
    _shard: PhantomData<Sh>,
}

impl<St, E, Sh> std::fmt::Debug for Dispatch<St, E, Sh>
where
    E: Encoding,
    Sh: tina::Shard,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatch")
            .field("state", &std::any::type_name::<St>())
            .field("encoder", &self.encoding.name())
            .field("methods", &self.table)
            .field("limits", &self.limits)
            .finish()
    }
}

impl<St, E, Sh> Dispatch<St, E, Sh>
where
    E: Encoding,
    Sh: tina::Shard,
{
    /// Builds a typed dispatch handler.
    ///
    /// `limits` bounds both the request decode and the reply
    /// encode for every method in `table`. Configure it alongside
    /// [`crate::FrameLimits::max_frame_size`] — the wire frame
    /// has to be able to carry the encoded payload.
    pub fn new(state: St, table: MethodTable<St, E>, encoding: E, limits: PayloadLimits) -> Self {
        Self {
            state,
            table,
            encoding,
            limits,
            _shard: PhantomData,
        }
    }

    /// Borrows the user state. There is intentionally no
    /// `state_mut` accessor — handler-private mutation routes
    /// through dispatched calls so behavior remains observable
    /// through the same path callers exercise.
    pub fn state(&self) -> &St {
        &self.state
    }

    /// Returns the configured payload-size limits.
    pub fn limits(&self) -> &PayloadLimits {
        &self.limits
    }
}

impl<St, E, Sh> ServiceHandler<Sh> for Dispatch<St, E, Sh>
where
    St: 'static,
    E: Encoding + 'static,
    Sh: tina::Shard + 'static,
{
    fn dispatch(&mut self, call: ServiceCall) -> ServiceReply {
        self.table
            .dispatch(&mut self.state, call, &self.encoding, &self.limits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde::{Deserialize, Serialize};

    use tina::ShardId;

    use crate::encoding::Json;

    #[derive(Debug, Default)]
    struct TestShard;

    impl tina::Shard for TestShard {
        fn id(&self) -> ShardId {
            ShardId::new(92)
        }
    }

    // ---- A small typed service for tests ----

    #[derive(Debug, Default)]
    struct Counter {
        value: i64,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Add {
        delta: i64,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct State {
        value: i64,
    }

    fn counter_table() -> MethodTable<Counter, Json> {
        MethodTable::new()
            .method(Method::new("add", |state: &mut Counter, req: Add| {
                state.value += req.delta;
                State { value: state.value }
            }))
            .method(Method::new("read", |state: &mut Counter, _: ()| State {
                value: state.value,
            }))
    }

    fn build_dispatch(max_bytes: usize) -> Dispatch<Counter, Json, TestShard> {
        Dispatch::new(
            Counter::default(),
            counter_table(),
            Json,
            PayloadLimits::symmetric(max_bytes),
        )
    }

    fn json_call(method: &str, payload: impl Serialize) -> ServiceCall {
        ServiceCall {
            method: method.into(),
            payload: serde_json::to_vec(&payload).expect("encode test payload"),
        }
    }

    fn raw_call(method: &str, payload: Vec<u8>) -> ServiceCall {
        ServiceCall {
            method: method.into(),
            payload,
        }
    }

    fn ok_payload<T: DeserializeOwned>(reply: ServiceReply) -> T {
        match reply {
            ServiceReply::Ok(bytes) => serde_json::from_slice(&bytes).expect("decode test reply"),
            other => panic!("expected ServiceReply::Ok, got {other:?}"),
        }
    }

    // ---- Tests ----

    #[test]
    fn known_method_round_trips_typed_request_and_response() {
        let mut svc = build_dispatch(4096);
        let reply = svc.dispatch(json_call("add", Add { delta: 5 }));
        let state: State = ok_payload(reply);
        assert_eq!(state.value, 5);
    }

    #[test]
    fn handler_state_persists_across_calls() {
        let mut svc = build_dispatch(4096);
        let _ = svc.dispatch(json_call("add", Add { delta: 3 }));
        let _ = svc.dispatch(json_call("add", Add { delta: 7 }));
        let reply = svc.dispatch(json_call("read", ()));
        let state: State = ok_payload(reply);
        assert_eq!(state.value, 10);
        assert_eq!(svc.state().value, 10);
    }

    #[test]
    fn unknown_method_returns_unknown_method() {
        let mut svc = build_dispatch(4096);
        let reply = svc.dispatch(json_call("not_a_method", ()));
        assert_eq!(reply, ServiceReply::UnknownMethod);
    }

    #[test]
    fn malformed_payload_returns_decode() {
        let mut svc = build_dispatch(4096);
        // The "add" method expects `{"delta": <i64>}`; send garbage JSON.
        let reply = svc.dispatch(raw_call("add", b"{not valid json".to_vec()));
        assert_eq!(reply, ServiceReply::Decode);
    }

    #[test]
    fn wrong_shape_payload_returns_decode() {
        let mut svc = build_dispatch(4096);
        // Valid JSON but wrong type (object missing `delta`).
        let reply = svc.dispatch(raw_call("add", b"{\"oops\": 1}".to_vec()));
        assert_eq!(reply, ServiceReply::Decode);
    }

    #[test]
    fn oversize_request_payload_returns_decode() {
        // max_payload smaller than the encoded `Add { delta: 5 }`
        // bytes (`{"delta":5}` = 11 bytes). The Encoding trait
        // rejects before the deserializer runs.
        let mut svc = build_dispatch(4);
        let reply = svc.dispatch(json_call("add", Add { delta: 5 }));
        assert_eq!(reply, ServiceReply::Decode);
    }

    #[test]
    fn oversize_reply_returns_internal() {
        // The `read` method's request type is `()` which JSON-encodes
        // as `null` (4 bytes); the reply `{"value":12345}` is 15
        // bytes. With `max_response = 4` the request decode fits
        // exactly while the reply encode overflows, isolating the
        // encode-cap path.
        let mut svc = Dispatch::<Counter, Json, TestShard>::new(
            Counter { value: 12345 },
            counter_table(),
            Json,
            PayloadLimits {
                max_request: 4,
                max_response: 4,
            },
        );
        let reply = svc.dispatch(raw_call("read", b"null".to_vec()));
        assert_eq!(reply, ServiceReply::Internal);
    }

    #[test]
    fn asymmetric_payload_limits_independently_bound_each_side() {
        // Tight on request (4 bytes), loose on response (1 KiB).
        // The `read` request `null` (4 bytes) fits; the reply
        // (~15 bytes) also fits. Both succeed.
        let mut svc = Dispatch::<Counter, Json, TestShard>::new(
            Counter { value: 7 },
            counter_table(),
            Json,
            PayloadLimits {
                max_request: 4,
                max_response: 1024,
            },
        );
        let reply = svc.dispatch(raw_call("read", b"null".to_vec()));
        let s: State = ok_payload(reply);
        assert_eq!(s.value, 7);

        // Loose on request, tight on response: the `add` request
        // `{"delta":1}` (11 bytes) decodes; the reply
        // `{"value":1}` (11 bytes) doesn't fit in `max_response = 4`.
        let mut svc = Dispatch::<Counter, Json, TestShard>::new(
            Counter::default(),
            counter_table(),
            Json,
            PayloadLimits {
                max_request: 1024,
                max_response: 4,
            },
        );
        let reply = svc.dispatch(json_call("add", Add { delta: 1 }));
        assert_eq!(reply, ServiceReply::Internal);
    }

    #[test]
    #[should_panic(expected = "duplicate method name 'ping'")]
    fn duplicate_method_name_panics() {
        // Re-adding the same name is a programming bug (typo in
        // the macro, hand-written table mistake) — fail loudly.
        let _: MethodTable<Counter, Json> = MethodTable::new()
            .method(Method::new("ping", |_: &mut Counter, _: ()| 1u8))
            .method(Method::new("ping", |_: &mut Counter, _: ()| 2u8));
    }

    #[test]
    fn handler_panic_returns_internal_and_isolate_stays_alive() {
        // A panic in user code surfaces as ServiceReply::Internal;
        // the dispatch returns rather than tearing the isolate
        // down. State may be poisoned (AssertUnwindSafe) but the
        // caller still gets a structured reply.
        let table: MethodTable<Counter, Json> = MethodTable::new()
            .method(Method::new("boom", |_: &mut Counter, _: ()| -> u8 {
                panic!("handler exploded")
            }));
        let mut svc = Dispatch::<Counter, Json, TestShard>::new(
            Counter::default(),
            table,
            Json,
            PayloadLimits::default(),
        );
        let reply = svc.dispatch(raw_call("boom", b"null".to_vec()));
        assert_eq!(reply, ServiceReply::Internal);

        // Subsequent calls still work — the isolate is alive.
        let table_after: MethodTable<Counter, Json> = MethodTable::new()
            .method(Method::new("read", |state: &mut Counter, _: ()| State {
                value: state.value,
            }));
        let mut svc = Dispatch::<Counter, Json, TestShard>::new(
            Counter::default(),
            table_after,
            Json,
            PayloadLimits::default(),
        );
        let reply = svc.dispatch(raw_call("read", b"null".to_vec()));
        let s: State = ok_payload(reply);
        assert_eq!(s.value, 0);
    }

    #[test]
    fn dispatch_is_a_service_handler() {
        // Compile-test: `Dispatch` satisfies `ServiceHandler` for
        // the chosen shard. This is what makes it droppable into
        // `SingleService`.
        fn assert_handler<H: ServiceHandler<TestShard>>(_h: &H) {}
        let svc = build_dispatch(4096);
        assert_handler(&svc);
    }

    #[test]
    fn dispatch_composes_with_single_service() {
        // End-to-end wiring: build a Dispatch, drop it into
        // SingleService (the single-isolate topology adapter),
        // and prove the resulting Tina isolate handler routes a
        // ServiceCall to ServiceReply via Effect::Reply.
        use crate::SingleService;
        use tina::{Context, Effect, Isolate, IsolateId};

        let svc = SingleService::<Dispatch<Counter, Json, TestShard>, TestShard>::new(
            build_dispatch(4096),
        );
        let mut svc = svc;
        let mut shard = TestShard;
        let mut ctx = Context::new(&mut shard, IsolateId::new(99));
        let effect = svc.handle(json_call("read", ()), &mut ctx);
        match effect {
            Effect::Reply(ServiceReply::Ok(bytes)) => {
                let s: State = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(s.value, 0);
            }
            other => panic!("expected Effect::Reply(Ok), got {other:?}"),
        }
    }

    #[test]
    fn empty_method_name_returns_unknown_method() {
        // Pin: an empty method name is just another unknown method.
        // Future code that special-cases "" (default method?)
        // would change this and should change deliberately.
        let mut svc = build_dispatch(4096);
        let reply = svc.dispatch(raw_call("", b"null".to_vec()));
        assert_eq!(reply, ServiceReply::UnknownMethod);
    }

    #[test]
    fn empty_table_returns_unknown_method_for_anything() {
        let table: MethodTable<Counter, Json> = MethodTable::new();
        assert!(table.is_empty());
        let mut state = Counter::default();
        let reply = table.dispatch(
            &mut state,
            json_call("anything", ()),
            &Json,
            &PayloadLimits::symmetric(64),
        );
        assert_eq!(reply, ServiceReply::UnknownMethod);
    }

    #[test]
    fn multiple_methods_route_independently() {
        let mut svc = build_dispatch(4096);
        let r1 = svc.dispatch(json_call("add", Add { delta: 100 }));
        let r2 = svc.dispatch(json_call("read", ()));
        let s1: State = ok_payload(r1);
        let s2: State = ok_payload(r2);
        assert_eq!(s1.value, 100);
        assert_eq!(s2.value, 100);
    }

    /// Result-shaped handler: the user method returns
    /// `Result<Ok, DomainError>`; both arms ride
    /// `ServiceReply::Ok(bytes)` because the encoding handles the
    /// `Result`. This pins the documented convention for
    /// domain-level errors.
    #[test]
    fn domain_error_rides_success_path() {
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        enum BillingError {
            InsufficientFunds,
        }
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        struct ChargeRequest {
            amount: i64,
        }
        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        struct Receipt {
            id: u64,
        }

        struct Wallet {
            balance: i64,
            next_receipt_id: u64,
        }

        let table: MethodTable<Wallet, Json> = MethodTable::new().method(Method::new(
            "charge",
            |state: &mut Wallet, req: ChargeRequest| -> Result<Receipt, BillingError> {
                if state.balance < req.amount {
                    return Err(BillingError::InsufficientFunds);
                }
                state.balance -= req.amount;
                state.next_receipt_id += 1;
                Ok(Receipt {
                    id: state.next_receipt_id,
                })
            },
        ));
        let mut svc = Dispatch::<Wallet, Json, TestShard>::new(
            Wallet {
                balance: 100,
                next_receipt_id: 0,
            },
            table,
            Json,
            PayloadLimits::symmetric(4096),
        );

        // Successful charge: the Ok variant rides ServiceReply::Ok.
        let reply = svc.dispatch(json_call("charge", ChargeRequest { amount: 30 }));
        let outcome: Result<Receipt, BillingError> = ok_payload(reply);
        assert_eq!(outcome, Ok(Receipt { id: 1 }));

        // Insufficient funds: Err(BillingError) ALSO rides
        // ServiceReply::Ok — the wire `Error` codes are reserved
        // for transport faults, not domain failures.
        let reply = svc.dispatch(json_call("charge", ChargeRequest { amount: 9999 }));
        let outcome: Result<Receipt, BillingError> = ok_payload(reply);
        assert_eq!(outcome, Err(BillingError::InsufficientFunds));
    }
}
