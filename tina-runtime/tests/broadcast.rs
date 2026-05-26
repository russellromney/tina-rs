//! End-to-end proof for bounded observed broadcast.

use std::convert::Infallible;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    BroadcastReport, BroadcastTargets, BroadcastTracker, DefaultThreadedMailboxFactory,
    SendOutcome, ThreadedRuntime, broadcast_observed,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeliverMsg(u8);

struct SlowSink;

#[tina::isolate(message = DeliverMsg)]
impl SlowSink {
    fn handle(
        &mut self,
        _msg: DeliverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverMsg {
    Start,
    Observed(u8, SendOutcome),
}

struct Driver {
    targets: Option<BroadcastTargets<u8, DeliverMsg>>,
    tracker: BroadcastTracker<u8>,
}

#[tina_runtime::isolate(
    message = DriverMsg,
    send = Outbound<DeliverMsg>,
)]
impl Driver {
    fn handle(
        &mut self,
        msg: DriverMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Start => {
                let targets = self.targets.take().expect("broadcast starts once");
                broadcast_observed(targets, |key| DeliverMsg(*key), DriverMsg::Observed)
            }
            DriverMsg::Observed(key, outcome) => match self.tracker.record(key, outcome) {
                Ok(Some(report)) => stop_with(report),
                Ok(None) => noop(),
                Err(error) => panic!("unexpected broadcast record error: {error:?}"),
            },
        }
    }
}

#[test]
fn broadcast_observed_reports_accepted_full_and_closed() {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let sink = runtime
        .register_with_capacity::<SlowSink, Infallible>(SlowSink, 1)
        .expect("register sink");
    let closed = Address::<DeliverMsg>::new(tina::ShardId::new(0), tina::IsolateId::new(99));
    let targets = BroadcastTargets::try_from_iter(3, [(0, sink), (1, sink), (2, closed)]).unwrap();
    let tracker = targets.tracker();
    let driver = runtime
        .register_with_capacity::<Driver, DeliverMsg>(
            Driver {
                targets: Some(targets),
                tracker,
            },
            8,
        )
        .expect("register driver");
    let result = runtime
        .observe_result::<BroadcastReport<u8>, _, _>(driver)
        .expect("observe result");

    runtime.try_send(driver, DriverMsg::Start).expect("start");
    let report = result
        .wait(Duration::from_secs(2))
        .expect("broadcast report");
    assert_eq!(report.accepted(), 1);
    assert_eq!(report.full(), 1);
    assert_eq!(report.closed(), 1);
    assert_eq!(report.max_targets(), 3);
    report.assert_all_accounted_for(3).unwrap();
    assert_eq!(
        report
            .outcomes()
            .iter()
            .map(|o| (o.key, o.outcome))
            .collect::<Vec<_>>(),
        vec![
            (0, SendOutcome::Accepted),
            (1, SendOutcome::Full),
            (2, SendOutcome::Closed),
        ]
    );

    let _ = runtime.shutdown();
}
