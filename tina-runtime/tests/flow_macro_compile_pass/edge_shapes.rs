use std::convert::Infallible;
use std::time::Duration;

use tina::{Outbound, noop, reply_to};
use tina_runtime::CallOutcome;

macro_rules! respond {
    ($req:expr) => {
        reply_to($req, 1)
    };
}

pub mod api {
    use super::*;

    mod request {
        pub type Outcome = u32;
    }

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

            step QualifiedRawType() -> raw request::Outcome {
                let _ = outcome;
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

// Soak-shaped proof: caller authority and a move-only lease travel directly
// through two typed timer events. There is no qid or pending-reply sidecar.
struct MoveOnlyLease;

enum SoakEvent {
    Flow(SoakFlow),
}

enum SoakRequest {
    Run,
}

struct SoakDriver;

tina::flow! {
    flow SoakFlow for SoakDriver {
        reply u32;

        step HttpReleased(http_lease: MoveOnlyLease) -> raw request tina_runtime::SleepReply {
            match outcome {
                Ok(()) if !req.is_open() => {
                    drop(http_lease);
                    noop()
                }
                Ok(()) => {
                    drop(http_lease);
                    let db_lease = MoveOnlyLease;
                    tina_runtime::sleep(Duration::from_millis(1))
                        .then_service_event_with_request(req, move |req, outcome| {
                            SoakEvent::Flow(SoakFlow::DbReleased(req, db_lease, outcome))
                        })
                }
                Err(_error) => {
                    drop(http_lease);
                    reply_to(req, 0)
                }
            }
        }

        step DbReleased(db_lease: MoveOnlyLease) -> raw request tina_runtime::SleepReply {
            drop(db_lease);
            match outcome {
                Ok(()) => reply_to(req, 1),
                Err(_error) => reply_to(req, 0),
            }
        }

        step FileClosed() -> raw request tina_runtime::CallReply<()> {
            match outcome {
                Ok(()) => reply_to(req, 1),
                Err(_error) => reply_to(req, 0),
            }
        }
    }
}

fn continue_typed_io_without_envelope(
    req: tina::RequestContext<u32>,
    file: tina_runtime::FileId,
) -> tina::Effect<SoakDriver> {
    tina_runtime::file_close(file).then_service_event_with_request(req, |req, outcome| {
        SoakEvent::Flow(SoakFlow::FileClosed(req, outcome))
    })
}

#[tina_runtime::isolate(event = SoakEvent, request = SoakRequest, reply = u32)]
impl SoakDriver {
    fn handle_event(
        &mut self,
        event: SoakEvent,
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        match event {
            SoakEvent::Flow(flow) => self.handle_soak_flow(flow),
        }
    }

    fn handle_request(
        &mut self,
        _request: SoakRequest,
        call: tina::RequestCall<'_, Self>,
    ) -> tina::RequestEffect<Self> {
        call.capture(|req| {
            let http_lease = MoveOnlyLease;
            tina_runtime::sleep(Duration::from_millis(1)).then_service_event_with_request(
                req,
                move |req, outcome| {
                    SoakEvent::Flow(SoakFlow::HttpReleased(req, http_lease, outcome))
                },
            )
        })
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

fn assert_qualified_raw_type_is_not_a_request_step(outcome: u32) -> api::MixedArrowFlow {
    api::MixedArrowFlow::QualifiedRawType(outcome)
}

fn main() {
    let _ = noop::<api::Driver>();
    let _ = dispatch_public;
    let _ = assert_acronym_variant;
    let _ = assert_call_step_shape;
    let _ = assert_raw_step_shape;
    let _ = assert_qualified_raw_type_is_not_a_request_step;
    let _ = SoakRequest::Run;
    let _ = continue_typed_io_without_envelope;
}
