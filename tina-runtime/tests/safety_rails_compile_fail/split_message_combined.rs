//! Negative fixture: split services cannot also declare a raw message type.

#[derive(Debug)]
enum Event {
    Filled,
}

#[derive(Debug)]
enum Request {
    Read,
}

#[derive(Debug)]
enum Message {
    Raw,
}

struct Service;

#[tina_runtime::isolate(message = Message, event = Event, request = Request, reply = ())]
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
