//! Negative fixture: split services must implement handle_event.

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
    fn handle_request(
        &mut self,
        _request: Request,
        call: tina::CallContext<'_, Self>,
    ) -> tina::Effect<Self> {
        call.reply(())
    }
}

fn main() {}
