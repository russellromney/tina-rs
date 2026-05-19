//! Negative fixture: split services cannot also be send_only.

#[derive(Debug)]
enum Event {
    Filled,
}

#[derive(Debug)]
enum Request {
    Read,
}

struct Service;

#[tina_runtime::isolate(event = Event, request = Request, send_only)]
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
        call.reply(())
    }
}

fn main() {}
