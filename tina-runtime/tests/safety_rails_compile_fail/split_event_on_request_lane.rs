//! Negative fixture: split service events cannot be called as requests.

#[derive(Debug)]
enum Event {
    Filled,
}

#[derive(Debug)]
enum Request {
    Read,
}

struct Producer {
    requests: tina::ServiceRequestAddress<Event, Request, u32>,
}

#[tina_runtime::isolate(message = (), send = tina::ServiceOutbound<Event, Request>)]
impl Producer {
    fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        // Expected `Request`, found `Event`.
        tina_runtime::call_request(
            self.requests,
            Event::Filled,
            std::time::Duration::from_millis(1),
        )
        .then(|_| ())
    }
}

fn main() {
}
