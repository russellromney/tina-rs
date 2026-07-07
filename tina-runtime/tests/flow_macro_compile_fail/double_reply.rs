use tina::{Outbound, noop, reply_to_request};
use tina_runtime::{CallOutcome, RuntimeCall};

struct Driver;

enum DriverMsg {
    Flow(DoubleReplyFlow),
}

tina::flow! {
    flow DoubleReplyFlow for Driver {
        reply u32;

        step Done() -> u32 {
            let first = reply_to_request(req, 1);
            let second = reply_to_request(req, 2);
            tina::batch(vec![first, second])
        }
    }
}

#[tina_runtime::isolate(
    message = DriverMsg,
    reply = u32,
    send = Outbound<std::convert::Infallible>,
    io = RuntimeCall<DriverMsg>
)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        match msg {
            DriverMsg::Flow(flow) => self.handle_double_reply_flow(flow),
        }
    }

    fn handle_call(
        &mut self,
        _msg: DriverMsg,
        call: tina::CallContext<'_, Self>,
    ) -> tina::Effect<Self> {
        call.reply(0)
    }
}

fn main() {
    let _ = CallOutcome::<u32>::Full;
    let _ = noop::<Driver>();
}
