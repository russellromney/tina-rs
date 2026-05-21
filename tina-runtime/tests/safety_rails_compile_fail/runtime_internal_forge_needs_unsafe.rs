//! Negative fixture (H4): the `runtime_internal` escape hatch through the
//! split-service must-answer rail is `unsafe`. Foreign/app code cannot forge a
//! `RequestEffect` from `noop()` via the safe path — calling the hatch without
//! an `unsafe` block does not compile, and `#![forbid(unsafe_code)]` crates
//! reject the `unsafe` form outright.

struct Demo;

#[tina_runtime::isolate(message = (), reply = ())]
impl Demo {
    fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        tina::noop()
    }
}

fn forge() -> tina::RequestEffect<Demo> {
    tina::runtime_internal::request_effect_from_consumed_effect(tina::noop())
}

fn main() {
    let _ = forge();
}
