use std::convert::Infallible;

use tina::{Outbound, noop, reply_to};
use tina_runtime::CallOutcome;

macro_rules! respond {
    ($req:expr) => {
        reply_to($req, 1)
    };
}

pub mod api {
    use super::*;

    pub struct Driver;

    pub enum DriverMsg {
        Flow(PublicHTTPFlow),
    }

    tina::flow! {
        pub flow PublicHTTPFlow for Driver {
            reply u32;

            step Done() -> u32 {
                let _scratch = 5u32;
                let _ = outcome;
                reply_to(req, 2)
            }
        }
    }

    tina::flow! {
        flow MacroReqFlow for Driver {
            reply u32;

            step Done() -> u32 {
                let _ = outcome;
                respond!(req)
            }
        }
    }

    tina::flow! {
        flow UnusedScratchFlow for Driver {
            reply u32;

            step Done() -> u32 {
                let scratch = 5u32;
                let _ = outcome;
                reply_to(req, 3)
            }
        }
    }

    #[tina_runtime::isolate(
        message = DriverMsg,
        reply = u32,
        send = Outbound<Infallible>,
        io = tina_runtime::RuntimeCall<DriverMsg>
    )]
    impl Driver {
        fn handle(
            &mut self,
            msg: DriverMsg,
            _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
        ) -> tina::Effect<Self> {
            match msg {
                DriverMsg::Flow(flow) => self.handle_public_http_flow(flow),
            }
        }
    }
}

fn dispatch_public(
    driver: &mut api::Driver,
    flow: api::PublicHTTPFlow,
) -> tina::Effect<api::Driver> {
    driver.handle_public_http_flow(flow)
}

fn assert_acronym_variant(
    req: tina::RequestContext<u32>,
    outcome: CallOutcome<u32>,
) -> api::PublicHTTPFlow {
    api::PublicHTTPFlow::Done(req, outcome)
}

fn main() {
    let _ = noop::<api::Driver>();
    let _ = dispatch_public;
    let _ = assert_acronym_variant;
}
