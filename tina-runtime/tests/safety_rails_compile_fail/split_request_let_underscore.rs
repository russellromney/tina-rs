//! Negative fixture: split request handlers must consume caller authority correctly.

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
        call: tina::RequestCall<'_, Self>,
    ) -> tina::RequestEffect<Self> {
        let _ = call;
        tina::noop()
    }
}

fn main() {}
