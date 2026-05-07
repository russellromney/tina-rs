//! Integration tests for `#[tina_rpc::service]`.
//!
//! Pin that the macro-generated server-side dispatcher and
//! client-side stub round-trip through the same `ServiceCall` /
//! `ServiceReply` / `ClientRequest` / `ClientResultMsg` shape the
//! raw API uses. The macro is pure expansion; these tests
//! exercise the expansion against a small typed service.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use tina::{Address, Context, Effect, Isolate, IsolateId, ShardId};
use tina_rpc::{
    ClientRequest, ClientResultMsg, PayloadLimits, ServiceCall, ServiceReply, SingleService,
    service,
};

#[derive(Debug, Default)]
struct TestShard;

impl tina::Shard for TestShard {
    fn id(&self) -> ShardId {
        ShardId::new(94)
    }
}

// ---------------------------------------------------------------------------
// A small typed service: a wallet with charge / refund / balance.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cents(pub i64);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiptId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub id: ReceiptId,
    pub charged: Cents,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BillingError {
    InsufficientFunds,
    UnknownReceipt,
}

#[service(name = "Billing")]
pub trait Billing {
    fn charge(&mut self, amount: Cents) -> Result<Receipt, BillingError>;
    fn refund(&mut self, id: ReceiptId) -> Result<Cents, BillingError>;
    /// Read-only — uses `&self` to prove the macro accepts both
    /// `&self` and `&mut self` receivers.
    fn balance(&self) -> Cents;
}

/// User implementation of the trait — what every `#[service]` user
/// writes.
#[derive(Debug, Default)]
struct Wallet {
    balance: Cents,
    next_id: u64,
    receipts: std::collections::HashMap<u64, Cents>,
}

impl Billing for Wallet {
    fn charge(&mut self, amount: Cents) -> Result<Receipt, BillingError> {
        if self.balance.0 < amount.0 {
            return Err(BillingError::InsufficientFunds);
        }
        self.balance.0 -= amount.0;
        self.next_id += 1;
        self.receipts.insert(self.next_id, amount);
        Ok(Receipt {
            id: ReceiptId(self.next_id),
            charged: amount,
        })
    }

    fn refund(&mut self, id: ReceiptId) -> Result<Cents, BillingError> {
        let amount = self
            .receipts
            .remove(&id.0)
            .ok_or(BillingError::UnknownReceipt)?;
        self.balance.0 += amount.0;
        Ok(amount)
    }

    fn balance(&self) -> Cents {
        self.balance
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn dispatch_through_single<H>(
    svc: &mut SingleService<tina_rpc::Dispatch<H, tina_rpc::Json, TestShard>, TestShard>,
    call: ServiceCall,
) -> ServiceReply
where
    H: 'static,
{
    let mut shard = TestShard;
    let mut ctx = Context::new(&mut shard, IsolateId::new(99));
    match svc.handle(call, &mut ctx) {
        Effect::Reply(reply) => reply,
        other => panic!("expected Effect::Reply, got {other:?}"),
    }
}

fn fake_reply_to() -> Address<ClientResultMsg> {
    Address::<ClientResultMsg>::new(ShardId::new(0), IsolateId::new(7))
}

fn build_service(
    starting_balance: Cents,
) -> SingleService<tina_rpc::Dispatch<Wallet, tina_rpc::Json, TestShard>, TestShard> {
    SingleService::new(BillingService::dispatch::<Wallet, TestShard>(
        Wallet {
            balance: starting_balance,
            ..Wallet::default()
        },
        PayloadLimits::default(),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn service_name_constant_round_trips() {
    assert_eq!(BillingService::SERVICE_NAME, "Billing");
    assert_eq!(BillingClient::SERVICE_NAME, "Billing");
    assert_eq!(BillingService::SERVICE_NAME, BillingClient::SERVICE_NAME);
}

#[test]
fn typed_round_trip_through_macro_generated_dispatch_and_client() {
    let mut svc = build_service(Cents(1_000));

    // Build a typed request via the generated client stub.
    let request: ClientRequest = BillingClient::charge_request(
        Cents(250),
        Duration::from_secs(1),
        /* correlator */ 42,
        fake_reply_to(),
        /* max_payload */ 1024,
    )
    .expect("encode charge request");
    assert_eq!(request.service, "Billing");
    assert_eq!(request.method, "charge");
    assert_eq!(request.correlator, 42);

    // Feed the wire-shaped ServiceCall through the dispatcher
    // (skipping the connection isolate; this test pins the
    // dispatcher's contract independent of TCP).
    let reply = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: request.method.clone(),
            payload: request.payload.clone(),
        },
    );
    let bytes = match reply {
        ServiceReply::Ok(bytes) => bytes,
        other => panic!("expected Ok, got {other:?}"),
    };

    // Decode the reply with the generated client decoder.
    let outcome: Result<Receipt, BillingError> =
        BillingClient::charge_decode_reply(&bytes, 1024).expect("decode reply");
    let receipt = outcome.expect("Wallet had balance");
    assert_eq!(receipt.charged, Cents(250));
    assert_eq!(receipt.id, ReceiptId(1));
}

#[test]
fn domain_error_rides_success_path() {
    // Wallet starts at 0; charging 1 fails with InsufficientFunds.
    // The Err arm rides ServiceReply::Ok bytes, not an Error frame.
    let mut svc = build_service(Cents(0));
    let request =
        BillingClient::charge_request(Cents(1), Duration::from_secs(1), 1, fake_reply_to(), 1024)
            .unwrap();
    let reply = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: request.method,
            payload: request.payload,
        },
    );
    let bytes = match reply {
        ServiceReply::Ok(bytes) => bytes,
        other => panic!("expected Ok-path even for domain error, got {other:?}"),
    };
    let outcome: Result<Receipt, BillingError> =
        BillingClient::charge_decode_reply(&bytes, 1024).unwrap();
    assert_eq!(outcome, Err(BillingError::InsufficientFunds));
}

#[test]
fn zero_arg_method_encodes_as_unit_tuple() {
    let mut svc = build_service(Cents(500));

    // `balance` takes no args; the generated client encodes `()`.
    let request =
        BillingClient::balance_request(Duration::from_secs(1), 1, fake_reply_to(), 1024).unwrap();
    assert_eq!(request.method, "balance");
    // JSON `null` is the documented `()` shape.
    assert_eq!(request.payload, b"null");

    let reply = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: request.method,
            payload: request.payload,
        },
    );
    let bytes = match reply {
        ServiceReply::Ok(b) => b,
        other => panic!("expected Ok, got {other:?}"),
    };
    let cents: Cents = BillingClient::balance_decode_reply(&bytes, 1024).unwrap();
    assert_eq!(cents, Cents(500));
}

#[test]
fn unknown_method_maps_to_unknown_method() {
    // Hand-craft a ServiceCall with a method name no trait method
    // matches. The macro-generated dispatcher must surface this as
    // ServiceReply::UnknownMethod (which the connection isolate
    // turns into wire Error(UnknownMethod)).
    let mut svc = build_service(Cents(0));
    let reply = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: "no_such_method".into(),
            payload: b"null".to_vec(),
        },
    );
    assert_eq!(reply, ServiceReply::UnknownMethod);
}

#[test]
fn malformed_payload_maps_to_decode() {
    let mut svc = build_service(Cents(1_000));
    // The "charge" method expects a tuple `(Cents,)` which JSON-
    // encodes as `[<cents>]`. Garbage bytes don't decode.
    let reply = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: "charge".into(),
            payload: b"not valid json".to_vec(),
        },
    );
    assert_eq!(reply, ServiceReply::Decode);
}

#[test]
fn oversize_request_payload_maps_to_decode_via_limits() {
    // Build a service with a tight request limit and prove the
    // decoder rejects before deserialization. `max_request = 1`
    // makes any non-empty payload reject; the reply path is irrelevant.
    let svc = SingleService::new(BillingService::dispatch::<Wallet, TestShard>(
        Wallet::default(),
        PayloadLimits {
            max_request: 1,
            max_response: 4096,
        },
    ));
    let mut svc = svc;
    let request =
        BillingClient::charge_request(Cents(1), Duration::from_secs(1), 1, fake_reply_to(), 4096)
            .unwrap();
    let reply = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: request.method,
            payload: request.payload,
        },
    );
    assert_eq!(reply, ServiceReply::Decode);
}

#[test]
fn oversize_reply_maps_to_internal_via_limits() {
    let svc = SingleService::new(BillingService::dispatch::<Wallet, TestShard>(
        Wallet {
            balance: Cents(99_999_999),
            ..Wallet::default()
        },
        PayloadLimits {
            max_request: 4096,
            // Tight on the reply: `99999999` doesn't fit in 4 bytes.
            max_response: 4,
        },
    ));
    let mut svc = svc;
    let request =
        BillingClient::balance_request(Duration::from_secs(1), 1, fake_reply_to(), 4096).unwrap();
    let reply = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: request.method,
            payload: request.payload,
        },
    );
    assert_eq!(reply, ServiceReply::Internal);
}

#[test]
fn handler_panic_maps_to_internal_via_dispatch_catch_unwind() {
    // A handler that panics on a specific input must surface as
    // ServiceReply::Internal — the dispatch core catches at the
    // boundary so a single bad request can't kill the isolate.
    // Define a separate trait + impl that panics on every charge.
    #[service]
    pub trait Panics {
        fn boom(&mut self) -> u8;
    }

    struct BoomImpl;
    impl Panics for BoomImpl {
        fn boom(&mut self) -> u8 {
            panic!("intentional handler panic");
        }
    }

    let mut svc = SingleService::new(PanicsService::dispatch::<BoomImpl, TestShard>(
        BoomImpl,
        PayloadLimits::default(),
    ));
    let request =
        PanicsClient::boom_request(Duration::from_secs(1), 1, fake_reply_to(), 1024).unwrap();
    let reply = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: request.method,
            payload: request.payload,
        },
    );
    assert_eq!(reply, ServiceReply::Internal);

    // The isolate is still alive; another method dispatches normally.
    let request2 =
        PanicsClient::boom_request(Duration::from_secs(1), 2, fake_reply_to(), 1024).unwrap();
    let reply2 = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: request2.method,
            payload: request2.payload,
        },
    );
    assert_eq!(reply2, ServiceReply::Internal);
}

#[test]
fn multi_arg_method_works() {
    // A trait method with two args proves the tuple encoding works
    // for non-trivial arg counts. This is the shape macro authors
    // care about most — anything more than zero or one args.
    #[service]
    pub trait Math {
        fn add(&mut self, a: i64, b: i64) -> i64;
    }

    struct Adder;
    impl Math for Adder {
        fn add(&mut self, a: i64, b: i64) -> i64 {
            a + b
        }
    }

    let mut svc = SingleService::new(MathService::dispatch::<Adder, TestShard>(
        Adder,
        PayloadLimits::default(),
    ));
    let request =
        MathClient::add_request(3, 4, Duration::from_secs(1), 1, fake_reply_to(), 1024).unwrap();
    let reply = dispatch_through_single(
        &mut svc,
        ServiceCall {
            method: request.method,
            payload: request.payload,
        },
    );
    let bytes = match reply {
        ServiceReply::Ok(b) => b,
        other => panic!("expected Ok, got {other:?}"),
    };
    let sum: i64 = MathClient::add_decode_reply(&bytes, 1024).unwrap();
    assert_eq!(sum, 7);
}

#[test]
fn multi_arg_payload_is_a_json_array() {
    // Pin the wire shape: multi-arg methods serialize as `[a, b]`,
    // single-arg as `[a]`, zero-arg as `null`.
    // Trait is unused as a trait — the macro only emits the
    // companion structs, which is what we exercise.
    #[allow(dead_code)]
    #[service]
    pub trait Wire {
        fn one(&mut self, x: i64) -> ();
        fn two(&mut self, a: i64, b: i64) -> ();
        fn zero(&mut self) -> ();
    }

    let one = WireClient::one_request(7, Duration::from_secs(1), 1, fake_reply_to(), 1024)
        .unwrap()
        .payload;
    assert_eq!(one, b"[7]");
    let two = WireClient::two_request(7, 8, Duration::from_secs(1), 1, fake_reply_to(), 1024)
        .unwrap()
        .payload;
    assert_eq!(two, b"[7,8]");
    let zero = WireClient::zero_request(Duration::from_secs(1), 1, fake_reply_to(), 1024)
        .unwrap()
        .payload;
    assert_eq!(zero, b"null");
}
