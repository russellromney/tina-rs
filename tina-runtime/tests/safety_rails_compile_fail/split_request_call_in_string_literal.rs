//! H8 negative fixture: mentioning `call` only inside a string literal must NOT
//! satisfy the must-use-authority gate. The old token-text scan passed this; the
//! AST visitor rejects it because the literal contents are never an expression.

#[derive(Debug)]
enum Event {
    Filled,
}

#[derive(Debug)]
enum Request {
    Read,
}

struct Service;

#[tina_runtime::isolate(event = Event, request = Request, reply = ())]
impl Service {
    fn handle_event(
        &mut self,
        _event: Event,
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        tina::noop()
    }

    fn handle_request(
        &mut self,
        _request: Request,
        call: tina::CallContext<'_, Self>,
    ) -> tina::Effect<Self> {
        let _note = "must answer the call somehow";
        tina::noop()
    }
}

fn main() {}
