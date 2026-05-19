//! Negative fixture: copied split request code cannot forge a RequestEffect from noop().

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
        let _ = &call;
        tina::RequestEffect::from_consumed_effect(tina::noop())
    }
}

fn main() {}
