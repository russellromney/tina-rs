//! Negative fixture: `request_effect_after_shared_wait(&ticket, effect)`
//! requires a real `&SharedWorkTicket<K>` borrow. Passing a non-ticket
//! is rejected at compile time, so user code cannot escape admission
//! by handing a `RequestEffect<I>` derived from plain `noop()`.

use std::convert::Infallible;

use tina::prelude::*;
use tina_runtime::request_effect_after_shared_wait;

#[derive(Debug)]
struct NoTicket;

impl Isolate for NoTicket {
    type Message = ();
    type Reply = ();
    type Send = tina::Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Call = Infallible;
    type Fact = Infallible;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        _: (),
        _: &mut tina::Context<'_, Self::Shard, Self::Reply>,
    ) -> tina::Effect<Self> {
        tina::noop()
    }
}

fn main() {
    let not_a_ticket: u32 = 0;
    let _ = request_effect_after_shared_wait::<NoTicket, u32>(&not_a_ticket, tina::noop());
}
