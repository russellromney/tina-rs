use std::convert::Infallible;

use tina::{Outbound, noop, reply_to};
use tina_runtime::RuntimeCall;

struct MoveOnlyLease;
struct Driver;

enum DriverMessage {
    Flow(RawRequestDoubleReplyFlow),
}

tina::flow! {
    flow RawRequestDoubleReplyFlow for Driver {
        reply u32;

        step Woke(lease: MoveOnlyLease) -> raw request tina_runtime::SleepReply {
            drop(lease);
            let first = reply_to(req, 1);
            let second = reply_to(req, 2);
            tina::batch([first, second])
        }
    }
}

#[tina_runtime::isolate(
    message = DriverMessage,
    reply = u32,
    send = Outbound<Infallible>,
    io = RuntimeCall<DriverMessage>
)]
impl Driver {
    fn handle(
        &mut self,
        message: DriverMessage,
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        match message {
            DriverMessage::Flow(flow) => self.handle_raw_request_double_reply_flow(flow),
        }
    }
}

fn main() {
    let _ = noop::<Driver>();
}
