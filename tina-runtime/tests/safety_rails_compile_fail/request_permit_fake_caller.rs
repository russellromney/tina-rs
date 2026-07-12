#![forbid(unsafe_code)]

use std::any::TypeId;
use std::rc::Rc;

fn main() {
    let registry = Rc::new(tina::DeferredSlotRegistry::new());
    let caller = tina::MessageCaller::new(
        registry,
        1,
        tina::IsolateId::new(1),
        tina::CallRouting::Local,
        TypeId::of::<()>(),
    );
    let mut shard = tina::SingleShard;
    let _ctx = tina::Context::<_, ()>::new_typed(&mut shard, tina::IsolateId::new(1))
        .with_caller(caller);
}
