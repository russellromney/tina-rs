use std::marker::PhantomData;

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

fn forge<'request>() -> tina::RequestEffectPermit<'request, Demo> {
    tina::RequestEffectPermit {
        _request: PhantomData,
    }
}

fn main() {
    let _ = forge();
}
