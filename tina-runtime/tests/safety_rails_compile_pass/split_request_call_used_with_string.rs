//! H8 positive fixture: a handler that genuinely uses `call` authority still
//! compiles even when a string literal also spells `call`. The AST visitor must
//! count the real expression-position use, not be confused by the literal.

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

struct Service;

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
        let _note = "must answer the call";
        match req {
            Request::Read => call.reply(Reply),
        }
    }
}

fn main() {}
