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

fn escape<'request>(
    permit: tina::RequestEffectPermit<'request, Demo>,
) -> tina::RequestEffectPermit<'static, Demo> {
    permit
}

fn main() {}
