#![forbid(unsafe_code)]

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

fn main() {
    let mut shard = tina::SingleShard;
    let context = tina::Context::<_, ()>::new_typed(&mut shard, tina::IsolateId::new(1));
    let _call = tina::CallContext::<Demo>::new(context);
}
