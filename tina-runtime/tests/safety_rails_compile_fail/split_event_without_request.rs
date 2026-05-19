//! Negative fixture: split services must declare event and request together.

#[derive(Debug)]
enum Event {
    Filled,
}

struct Service;

#[tina_runtime::isolate(event = Event, reply = ())]
impl Service {
    fn handle_event(
        &mut self,
        _event: Event,
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        tina::noop()
    }
}

fn main() {}
