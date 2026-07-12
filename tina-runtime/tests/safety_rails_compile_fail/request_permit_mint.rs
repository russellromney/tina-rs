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

fn mint<'request>() -> tina::RequestEffectPermit<'request, Demo> {
    tina::RequestEffectPermit::new()
}

fn main() {
    let _ = mint();
}
