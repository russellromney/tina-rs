use tina::prelude::*;

#[derive(Debug)]
enum Event {
    Tick,
}

struct Service;

#[tina_runtime::isolate(event = Event)]
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
}

fn main() {}
