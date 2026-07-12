enum Event {
    Tick,
}

struct Service;

#[tina_runtime::isolate(event = Event, reply = u32)]
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
