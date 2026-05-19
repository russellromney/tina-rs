//! Negative fixture: an ordinary isolate (`Fact = Infallible`) cannot emit a
//! protocol fact. The `tina::fact::<Self>(...)` call site requires the value's
//! type to be exactly `I::Fact`, which is `Infallible`. There is no
//! `ProtocolFact -> Infallible` conversion, so the call fails to type-check.

use std::convert::Infallible;

use tina::prelude::*;
use tina_runtime::{
    Http2StreamId, ProtocolConnectionId, ProtocolDirection, ProtocolFact, RuntimeCall,
};

#[derive(Debug)]
enum Msg {
    Tick,
}

struct OrdinaryIsolate;

// No `fact = ProtocolFact` here; the macro defaults to `Infallible`.
#[tina_runtime::isolate(message = Msg)]
impl OrdinaryIsolate {
    fn handle(
        &mut self,
        _msg: Msg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        // BAD: emitting a ProtocolFact requires `Fact = ProtocolFact`.
        tina::fact::<Self>(ProtocolFact::Http2StreamOpened {
            connection: ProtocolConnectionId::new(0),
            stream: Http2StreamId::new(1),
            direction: ProtocolDirection::Inbound,
        })
    }
}

fn main() {}
