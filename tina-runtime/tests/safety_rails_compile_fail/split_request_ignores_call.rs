//! Negative fixture: split request handlers must use caller authority.
//!
//! This is not a perfect linear type proof, but it catches the common copied
//! mistake: a request handler receives `call` and returns `noop()`.

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
        tina::noop()
    }
}

fn main() {}
