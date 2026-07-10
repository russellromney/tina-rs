//! Focused test for [`tina_runtime::ThreadedMultiShardRuntime::call_blocking_request`].
//!
//! Multi-shard companion to `ThreadedRuntime::call_blocking_request` (see
//! `tests/safety_rails.rs`'s single-shard split-service coverage).
//! `system_session_auth` used to send a raw `tina::ServiceMessage::Request`
//! envelope through plain `call_blocking` because this helper did not
//! exist on the multi-shard runtime. This test proves the wrapped form
//! dispatches to `handle_request` (not `handle_event`) and carries the
//! typed reply back, routed to the shard that owns the address exactly
//! like `call_blocking`.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedMultiShardRuntime};

#[derive(Debug)]
enum BucketEvent {}

#[derive(Debug)]
enum BucketRequest {
    Get,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BucketReply(u32);

struct Bucket {
    value: u32,
}

#[tina_runtime::isolate(event = BucketEvent, request = BucketRequest, reply = BucketReply)]
impl Bucket {
    fn handle_event(
        &mut self,
        event: BucketEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {}
    }

    fn handle_request(
        &mut self,
        request: BucketRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            BucketRequest::Get => call.reply(BucketReply(self.value)),
        }
    }
}

#[test]
fn call_blocking_request_dispatches_to_handle_request_on_owning_shard() {
    let runtime = ThreadedMultiShardRuntime::new([SingleShard], DefaultThreadedMailboxFactory);
    let addr = runtime
        .register_with_capacity_on::<Bucket, Infallible>(SingleShard::ID, Bucket { value: 42 }, 8)
        .expect("register bucket");
    let requests = tina::ServiceRequestAddress::from_call_address(addr.callable());

    let outcome = runtime
        .call_blocking_request(requests, BucketRequest::Get, Duration::from_secs(2))
        .expect("call_blocking_request");

    assert_eq!(outcome, CallOutcome::Replied(BucketReply(42)));
    let _ = runtime.shutdown();
}
