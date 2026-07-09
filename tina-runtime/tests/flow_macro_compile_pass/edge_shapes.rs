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

    // Exercises the `-> raw T` arrow next to a `-> T` call step in one flow.
    // A raw step carries only its captures and `T` verbatim (no
    // `RequestContext` slot, no `CallOutcome` wrap) and its body need not
    // mention `req`; the call step keeps the original shape. The variant
    // shapes are pinned by `assert_call_step_shape` / `assert_raw_step_shape`
    // below, so a codegen regression that wrapped a raw step in
    // `RequestContext`/`CallOutcome` would fail to compile here.
    tina::flow! {
        pub flow MixedArrowFlow for Driver {
            reply u32;

            step Called(seed: u32) -> u32 {
                let _ = (seed, outcome);
                reply_to(req, 1)
            }

            step Woke(seed: u32) -> raw u32 {
                // No `req` in scope; the body compiles without mentioning it.
                let _ = (seed, outcome);
                noop()
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

// A call step's variant is `(RequestContext, captures.., CallOutcome<T>)`.
fn assert_call_step_shape(
    req: tina::RequestContext<u32>,
    seed: u32,
    outcome: CallOutcome<u32>,
) -> api::MixedArrowFlow {
    api::MixedArrowFlow::Called(req, seed, outcome)
}

// A raw step's variant is `(captures.., T)` — no `RequestContext`, no
// `CallOutcome` wrap. This will not compile if raw codegen regresses to the
// call shape.
fn assert_raw_step_shape(seed: u32, woke: u32) -> api::MixedArrowFlow {
    api::MixedArrowFlow::Woke(seed, woke)
}

fn main() {
    let _ = noop::<api::Driver>();
    let _ = dispatch_public;
    let _ = assert_acronym_variant;
    let _ = assert_call_step_shape;
    let _ = assert_raw_step_shape;
}
