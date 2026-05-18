//! Negative fixture: a protocol isolate that declares `Fact = ProtocolFact`
//! cannot emit some other fact enum through `tina::fact::<Self>(...)`. The
//! associated-type value must match exactly.

use std::convert::Infallible;

use tina::prelude::*;
use tina_runtime::{ProtocolFact, RuntimeCall};

#[derive(Debug)]
enum Msg {
    Tick,
}

/// A "fact" enum from somewhere else that is NOT `ProtocolFact`.
#[derive(Debug, Clone, Copy)]
enum WrongFact {
    Bogus,
}

struct ProtocolIsolate;

#[tina_runtime::isolate(message = Msg, fact = ProtocolFact)]
impl ProtocolIsolate {
    fn handle(
        &mut self,
        _msg: Msg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        // BAD: WrongFact is not ProtocolFact.
        tina::fact::<Self>(WrongFact::Bogus)
    }
}

fn main() {}
