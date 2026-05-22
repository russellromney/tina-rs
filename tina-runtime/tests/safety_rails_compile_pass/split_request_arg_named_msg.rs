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
        msg: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match req {
            Request::Read => msg.reply(Reply),
        }
    }
}

fn main() {}
