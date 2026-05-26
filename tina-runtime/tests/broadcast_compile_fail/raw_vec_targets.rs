use tina::prelude::*;
use tina_runtime::{SendOutcome, broadcast_observed};

#[derive(Debug, Clone)]
enum Msg {
    Deliver(u8),
    Done(u8, SendOutcome),
}

struct Owner;

#[tina_runtime::isolate(message = Msg, send = Outbound<Msg>)]
impl Owner {
    fn handle(&mut self, _msg: Msg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        noop()
    }
}

fn main() {
    let target = Address::<Msg>::new(ShardId::new(0), IsolateId::new(1));
    let raw_targets = vec![(1_u8, target)];

    let _effect: Effect<Owner> =
        broadcast_observed(raw_targets, |key| Msg::Deliver(*key), Msg::Done);
}
