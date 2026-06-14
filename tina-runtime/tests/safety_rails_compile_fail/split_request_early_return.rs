//! Negative fixture: early `return` inside a split request handler must still
//! be checked against `RequestEffect`.

use tina::prelude::*;

#[derive(Debug)]
enum Event {
    Tick,
}

#[derive(Debug)]
enum Request {
    Read,
}

#[derive(Debug, Clone)]
struct Reply;

struct Service {
    draining: bool,
}

#[tina_runtime::isolate(event = Event, request = Request, reply = Reply)]
impl Service {
    fn handle_event(
        &mut self,
        event: Event,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            Event::Tick => noop(),
        }
    }

    fn handle_request(
        &mut self,
        req: Request,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        if self.draining {
            return noop();
        }
        match req {
            Request::Read => call.reply(Reply),
        }
    }
}

fn main() {}
