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

fn reuse(permit: tina::RequestEffectPermit<'_, Demo>) {
    let _first = permit.apply(tina::noop());
    let _second = permit.apply(tina::noop());
}

fn main() {}
