//! Prove `tina::sequence(...)` is equivalent to `tina::batch(...)`
//! and behaves the same on common shapes (empty, single, multi,
//! with `stop()`).

use std::convert::Infallible;

use tina::prelude::*;

#[derive(Debug, Default)]
struct Sink;

#[derive(Debug, Clone, Copy)]
enum SinkMsg {
    Tick(u32),
}

impl Isolate for Sink {
    type Message = SinkMsg;
    type Reply = ();
    type Send = Outbound<SinkMsg>;
    type Spawn = Infallible;
    type SpawnObserved = std::convert::Infallible;
    type Call = Infallible;
    type Shard = SingleShard;

    fn handle(
        &mut self,
        _msg: SinkMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

#[test]
fn sequence_of_empty_collapses_to_noop_shape() {
    let effects = sequence::<Sink, _>(Vec::<Effect<Sink>>::new());
    match effects {
        Effect::Batch(items) => assert!(items.is_empty()),
        other => panic!("expected Batch with empty Vec, got {other:?}"),
    }
}

#[test]
fn sequence_preserves_order() {
    let address = SingleShard.address::<SinkMsg>(IsolateId::new(1));
    let combined = sequence::<Sink, _>(vec![
        send(address, SinkMsg::Tick(1)),
        send(address, SinkMsg::Tick(2)),
        send(address, SinkMsg::Tick(3)),
    ]);
    let Effect::Batch(items) = combined else {
        panic!("expected Batch");
    };
    assert_eq!(items.len(), 3);
    // Each item is Send(_), check the Send order matches the input order.
    let ticks: Vec<u32> = items
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(out) => match out.message() {
                SinkMsg::Tick(n) => Some(*n),
            },
            _ => None,
        })
        .collect();
    assert_eq!(ticks, vec![1, 2, 3]);
}

#[test]
fn sequence_and_batch_produce_identical_shapes() {
    let address = SingleShard.address::<SinkMsg>(IsolateId::new(2));
    let one_way: Effect<Sink> = sequence(vec![send(address, SinkMsg::Tick(7))]);
    let other_way: Effect<Sink> = batch(vec![send(address, SinkMsg::Tick(7))]);
    let Effect::Batch(a) = one_way else {
        panic!("sequence -> Batch");
    };
    let Effect::Batch(b) = other_way else {
        panic!("batch -> Batch");
    };
    assert_eq!(a.len(), b.len());
}

#[test]
fn sequence_preserves_stop_position() {
    let address = SingleShard.address::<SinkMsg>(IsolateId::new(3));
    let combined: Effect<Sink> = sequence(vec![
        send(address, SinkMsg::Tick(1)),
        stop::<Sink>(),
        send(address, SinkMsg::Tick(2)),
    ]);
    let Effect::Batch(items) = combined else {
        panic!("expected Batch");
    };
    assert!(matches!(items[1], Effect::Stop));
}
