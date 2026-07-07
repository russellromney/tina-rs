//! Negative fixture: an isolate whose `Fact` type does not implement
//! `IntoRuntimeFact` cannot be registered with the runtime. The trait bound
//! at registration enforces that any fact an isolate actually carries can be
//! converted into the canonical `RuntimeFact` form.

use std::convert::Infallible;

use tina::prelude::*;
use tina_runtime::{DefaultMailboxFactory, Runtime};

/// A user-defined type that does not implement `IntoRuntimeFact`.
#[derive(Debug, Clone, Copy)]
struct UnregisterableFact;

#[derive(Debug)]
enum Msg {
    Tick,
}

struct BadFactIsolate;

// Manual `Isolate` impl with `type Fact = UnregisterableFact`. The macro
// would not let us write this without supplying a working type, so we keep
// the manual form for the negative fixture.
impl tina::Isolate for BadFactIsolate {
    type Message = Msg;
    type Reply = ();
    type Send = tina::Outbound<Infallible>;
    type Spawn = Infallible;
    type SpawnObserved = Infallible;
    type Io = Infallible;
    type Fact = UnregisterableFact;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        _msg: Msg,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        tina::noop()
    }
}

fn main() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    // BAD: UnregisterableFact: IntoRuntimeFact is not implemented.
    let _ = runtime.register_with_capacity::<BadFactIsolate, Infallible>(BadFactIsolate, 4);
}
