extern crate tina as renamed_tina;
extern crate tina_runtime as renamed_tina_runtime;

use std::convert::Infallible;

use renamed_tina::{Outbound, noop};

struct Driver;

enum DriverMsg {
    Flow(RenamedPathFlow),
}

renamed_tina::flow! {
    flow RenamedPathFlow for Driver {
        tina_crate = ::renamed_tina;
        runtime_crate = ::renamed_tina_runtime;
        reply u32;

        step Done(original: u32) -> u32 {
            match outcome {
                ::renamed_tina_runtime::CallOutcome::Replied(value) => {
                    ::renamed_tina::reply_to(req, original + value)
                }
                _ => ::renamed_tina::reply_to(req, 0),
            }
        }
    }
}

#[renamed_tina_runtime::isolate(
    message = DriverMsg,
    reply = u32,
    send = Outbound<Infallible>,
    io = renamed_tina_runtime::RuntimeCall<DriverMsg>
)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut renamed_tina::Context<'_, renamed_tina::SingleShard, Self::Reply>,
    ) -> renamed_tina::Effect<Self> {
        match msg {
            DriverMsg::Flow(flow) => self.handle_renamed_path_flow(flow),
        }
    }
}

fn assert_expansion_shape(
    req: renamed_tina::RequestContext<u32>,
    outcome: renamed_tina_runtime::CallOutcome<u32>,
) {
    let _ = RenamedPathFlow::Done(req, 40, outcome);
}

fn main() {
    let _ = noop::<Driver>();
}
