struct First;
struct Second;

#[tina_runtime::isolate(message = (), reply = ())]
impl First {
    fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        tina::noop()
    }
}

#[tina_runtime::isolate(message = (), reply = ())]
impl Second {
    fn handle(
        &mut self,
        _msg: (),
        _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
    ) -> tina::Effect<Self> {
        tina::noop()
    }
}

fn substitute(permit: tina::RequestEffectPermit<'_, First>) {
    let _wrong: tina::RequestEffect<Second> = permit.apply(tina::noop::<Second>());
}

fn main() {}
