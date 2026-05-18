//! Negative fixture: split services must implement handle_request.

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
}

fn main() {}
