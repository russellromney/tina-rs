use tina::{IsolateId, ShardId};
use tina_runtime::{CauseId, EventId, RuntimeEvent, RuntimeEventKind, SendRejectedReason};
use tina_sim::dst::{DstRun, History, InvariantSuite, ShrinkConfig, assert_replays, delete_shrink};

fn event(id: u64, cause: Option<u64>, kind: RuntimeEventKind) -> RuntimeEvent {
    RuntimeEvent::new(
        EventId::new(id),
        cause.map(|raw| CauseId::new(EventId::new(raw))),
        ShardId::new(1),
        IsolateId::new(1),
        kind,
    )
}

#[test]
fn dst_harness_replays_same_history_and_shrinks_failure() {
    let history = History::new("tiny arithmetic", 7, vec![1_u64, 2, 3, 4, 5]);
    let run = assert_replays(&history, |history| {
        let total = history.operations().iter().sum::<u64>() + history.seed();
        DstRun::new(total, format!("seed={}", history.seed()))
    });
    assert_eq!(*run.output(), 22);

    let shrunk = delete_shrink(
        &history,
        ShrinkConfig::default(),
        "sum remains above ten",
        |candidate| candidate.operations().iter().sum::<u64>() + candidate.seed() > 10,
    );
    assert!(shrunk.shrunk().len() < history.len());
    assert_eq!(shrunk.reason(), "sum remains above ten");
}

#[test]
fn common_invariants_catch_broken_causality_fixture() {
    let trace = vec![
        event(1, None, RuntimeEventKind::HandlerStarted),
        event(
            2,
            Some(99),
            RuntimeEventKind::SendRejected {
                target_shard: ShardId::new(1),
                target_isolate: IsolateId::new(2),
                target_generation: tina::AddressGeneration::new(0),
                reason: SendRejectedReason::Closed,
            },
        ),
    ];

    let failure = InvariantSuite::standard()
        .check(&trace)
        .expect_err("broken cause should fail");
    assert_eq!(failure.invariant(), "causes_point_backward");
    assert_eq!(failure.event_id(), Some(EventId::new(2)));
}

#[test]
fn common_invariants_accept_minimal_settled_send_trace() {
    let target_generation = tina::AddressGeneration::new(0);
    let trace = vec![
        event(
            1,
            None,
            RuntimeEventKind::SendDispatchAttempted {
                target_shard: ShardId::new(1),
                target_isolate: IsolateId::new(2),
                target_generation,
            },
        ),
        event(
            2,
            Some(1),
            RuntimeEventKind::SendAccepted {
                target_shard: ShardId::new(1),
                target_isolate: IsolateId::new(2),
                target_generation,
            },
        ),
    ];

    InvariantSuite::standard().check(&trace).unwrap();
}
