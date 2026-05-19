//! Negative fixture: split service requests cannot be sent on the event lane.

#[derive(Debug)]
enum Event {
    Filled,
}

#[derive(Debug)]
enum Request {
    Read,
}

struct Producer {
    events: tina::ServiceEventAddress<Event, Request>,
}

#[tina_runtime::isolate(message = (), send = tina::Outbound<tina::ServiceMessage<Event, Request>>)]
impl Producer {
    fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        // Expected `Event`, found `Request`.
        tina::send_event(self.events, Request::Read)
    }
}

fn main() {}
