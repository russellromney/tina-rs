//! Negative fixture: an isolate without `handle_call` cannot register as a
//! callable service. The expected diagnostic must include both the
//! "is not a callable service" message and the "missing `fn handle_call`"
//! label produced by `#[diagnostic::on_unimplemented]` on
//! [`tina::CallableIsolate`].

use std::convert::Infallible;

use tina::prelude::*;
use tina_runtime::{DefaultMailboxFactory, Runtime};

#[derive(Debug)]
enum Msg {
    Tick,
}

struct NoCallHandler;

#[tina_runtime::isolate(message = Msg)]
impl NoCallHandler {
    fn handle(
        &mut self,
        _msg: Msg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

fn main() {
    let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    let _ = runtime.register_service::<NoCallHandler, Infallible>(NoCallHandler, 4);
}
