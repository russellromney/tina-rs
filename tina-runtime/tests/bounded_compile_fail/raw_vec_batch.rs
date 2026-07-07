use std::convert::Infallible;

use tina::prelude::*;
use tina_runtime::bounded_batch;

#[derive(Debug, Clone)]
enum Msg {
    Start,
}

struct Owner;

impl Isolate for Owner {
    tina::isolate_types! {
        message: Msg,
        reply: (),
        send: Infallible,
        spawn: Infallible,
        io: Infallible,
        shard: SingleShard,
    }

    fn handle(&mut self, _msg: Msg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        noop()
    }
}

fn main() {
    let raw: Vec<Effect<Owner>> = vec![noop()];
    let _ = bounded_batch(raw);
}
