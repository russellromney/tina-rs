use std::convert::Infallible;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    CallError, CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, SharedWork,
    SharedWorkError, SleepReply, ThreadedRuntimeError, request_effect_after_shared_wait, sleep,
};

use crate::{BATCH_SIZE, BATCH_TIMEOUT_MS, CALLERS, MAX_PENDING, Report, SUBMISSION_CAPACITY};

const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum BatcherRequest {
    Submit(u64),
    Stats,
}

#[derive(Debug)]
enum BatcherEvent {
    Tick(u64, SleepReply),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatcherReply {
    Batched(u64),
    Full,
    TimerFailed(CallError),
    Stats(BatcherStats),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BatcherStats {
    current_generation: u64,
    items: usize,
    waiters: usize,
    waiter_high_water: usize,
    full_rejects: u64,
    reclaimed_callers: u64,
    size_flushes: usize,
    timer_flushes: usize,
    timer_failures: usize,
    stale_ticks: usize,
}

struct Batcher {
    waiters: SharedWork<u64, BatcherReply>,
    interval: Duration,
    batch_size: usize,
    items: Vec<u64>,
    generation: u64,
    pending_timer_generation: Option<u64>,
    size_flushes: usize,
    timer_flushes: usize,
    timer_failures: usize,
    stale_ticks: usize,
}

#[tina_runtime::isolate(event = BatcherEvent, request = BatcherRequest, reply = BatcherReply)]
impl Batcher {
    fn handle_event(
        &mut self,
        event: BatcherEvent,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match event {
            BatcherEvent::Tick(generation, result) => {
                if self.pending_timer_generation != Some(generation) {
                    self.stale_ticks += 1;
                    return noop();
                }
                self.pending_timer_generation = None;
                if let Err(error) = result {
                    self.timer_failures += 1;
                    return self.fail(generation, error);
                }
                if self.items.is_empty() {
                    return noop();
                }
                self.timer_flushes += 1;
                self.flush(generation)
            }
        }
    }

    fn handle_request(
        &mut self,
        request: BatcherRequest,
        call: RequestCall<'_, Self>,
    ) -> RequestEffect<Self> {
        match request {
            BatcherRequest::Submit(item) => self.submit(item, call),
            BatcherRequest::Stats => call.reply(BatcherReply::Stats(self.stats())),
        }
    }
}

impl Batcher {
    fn new(batch_size: usize, interval: Duration, max_pending: usize) -> Self {
        assert!(batch_size > 0, "batch size must be positive");
        Self {
            waiters: SharedWork::with_capacity(max_pending).named("bounded_batcher.waiters"),
            interval,
            batch_size,
            items: Vec::with_capacity(batch_size),
            generation: 1,
            pending_timer_generation: None,
            size_flushes: 0,
            timer_flushes: 0,
            timer_failures: 0,
            stale_ticks: 0,
        }
    }

    fn submit(&mut self, item: u64, call: RequestCall<'_, Self>) -> RequestEffect<Self> {
        let generation = self.generation;
        match self.waiters.wait(generation, call) {
            Ok((_ticket, permit)) => {
                self.items.push(item);
                if self.items.len() >= self.batch_size {
                    self.size_flushes += 1;
                    self.pending_timer_generation = None;
                    return request_effect_after_shared_wait(permit, self.flush(generation));
                }

                if self.pending_timer_generation.is_none() {
                    self.pending_timer_generation = Some(generation);
                    let tick = sleep(self.interval)
                        .then_service_event(move |result| BatcherEvent::Tick(generation, result));
                    return request_effect_after_shared_wait(permit, tick);
                }
                request_effect_after_shared_wait(permit, noop())
            }
            Err(SharedWorkError::Full { call, .. })
            | Err(SharedWorkError::KeyFull { call, .. }) => call.reply(BatcherReply::Full),
        }
    }

    fn flush(&mut self, generation: u64) -> Effect<Self> {
        let total = self.items.iter().sum();
        self.items.clear();
        self.generation += 1;
        Effect::Batch(
            self.waiters
                .reply_all_clone::<Self>(&generation, BatcherReply::Batched(total)),
        )
    }

    fn fail(&mut self, generation: u64, error: CallError) -> Effect<Self> {
        self.items.clear();
        self.generation += 1;
        Effect::Batch(
            self.waiters
                .reply_all_clone::<Self>(&generation, BatcherReply::TimerFailed(error)),
        )
    }

    fn stats(&self) -> BatcherStats {
        BatcherStats {
            current_generation: self.generation,
            items: self.items.len(),
            waiters: self.waiters.len(),
            waiter_high_water: self.waiters.high_water(),
            full_rejects: self.waiters.full_rejects(),
            reclaimed_callers: self.waiters.reclaimed(),
            size_flushes: self.size_flushes,
            timer_flushes: self.timer_flushes,
            timer_failures: self.timer_failures,
            stale_ticks: self.stale_ticks,
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let app = Arc::new(
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?,
    );
    let shutdown = app.shutdown_handle();
    let batcher = app
        .register_split_service::<Batcher, BatcherEvent, BatcherRequest, Infallible>(
            Batcher::new(
                BATCH_SIZE,
                Duration::from_millis(BATCH_TIMEOUT_MS),
                MAX_PENDING,
            ),
            SUBMISSION_CAPACITY,
        )
        .map_err(|error| anyhow::anyhow!("register batcher: {error:?}"))?;

    let mut callers = Vec::with_capacity(CALLERS);
    for item in 1..=CALLERS as u64 {
        let app = Arc::clone(&app);
        let requests = batcher.requests;
        callers.push(thread::spawn(move || {
            app.call_blocking_request(requests, BatcherRequest::Submit(item), CALL_TIMEOUT)
        }));
    }

    let mut report = Report {
        callers: CALLERS,
        ..Report::default()
    };
    for caller in callers {
        match caller
            .join()
            .map_err(|_| anyhow::anyhow!("batcher caller thread panicked"))?
        {
            Ok(outcome) => match outcome {
                CallOutcome::Replied(BatcherReply::Batched(_)) => report.successes += 1,
                CallOutcome::Replied(BatcherReply::Full) => report.full_rejects += 1,
                CallOutcome::Replied(BatcherReply::TimerFailed(_)) => {
                    report.timer_failures += 1;
                    report.failed += 1;
                }
                CallOutcome::Replied(BatcherReply::Stats(_)) => report.failed += 1,
                CallOutcome::Full => {
                    report.transport_full += 1;
                    report.failed += 1;
                }
                CallOutcome::Closed => {
                    report.closed += 1;
                    report.failed += 1;
                }
                CallOutcome::Timeout => {
                    report.timeouts += 1;
                    report.failed += 1;
                }
                CallOutcome::Rejected(_) => {
                    report.rejected += 1;
                    report.failed += 1;
                }
            },
            Err(error) => {
                record_host_error(&mut report, error);
                report.failed += 1;
            }
        }
    }
    let stats =
        match app.call_blocking_request(batcher.requests, BatcherRequest::Stats, CALL_TIMEOUT)? {
            CallOutcome::Replied(BatcherReply::Stats(stats)) => stats,
            CallOutcome::Replied(other) => anyhow::bail!("stats returned wrong reply: {other:?}"),
            CallOutcome::Full => anyhow::bail!("stats call mailbox was full"),
            CallOutcome::Closed => anyhow::bail!("batcher closed before stats"),
            CallOutcome::Timeout => anyhow::bail!("stats call timed out"),
            CallOutcome::Rejected(reason) => anyhow::bail!("stats call rejected: {reason:?}"),
        };
    report.batches_size_flushed = stats.size_flushes;
    report.batches_timer_flushed = stats.timer_flushes;

    let terminal = shutdown.request_and_wait_report(SHUTDOWN_TIMEOUT)?;
    drop(app);
    terminal.ensure_clean()?;
    report.exit_clean = true;
    Ok(report)
}

fn record_host_error(report: &mut Report, error: ThreadedRuntimeError) {
    match error {
        ThreadedRuntimeError::CommandFull => report.host_command_full += 1,
        ThreadedRuntimeError::WorkerStopped => report.host_worker_stopped += 1,
        ThreadedRuntimeError::HostWaitTimeout => report.host_wait_timeout += 1,
        ThreadedRuntimeError::WorkerUnresponsive => report.host_worker_unresponsive += 1,
        ThreadedRuntimeError::UnknownShard(_) => report.host_unknown_shard += 1,
        ThreadedRuntimeError::DriverShutdownFailed => report.host_driver_shutdown_failed += 1,
        ThreadedRuntimeError::DriverParkFailed => report.host_driver_park_failed += 1,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Instant;

    use super::*;

    type App = LocalSystem<SingleShard, DefaultThreadedMailboxFactory>;
    type Requests = tina::ServiceRequestAddress<BatcherEvent, BatcherRequest, BatcherReply>;

    struct Harness {
        app: Arc<App>,
        shutdown: tina_runtime::ThreadedShutdownHandle,
        service: tina_runtime::SplitServiceHandle<BatcherEvent, BatcherRequest, BatcherReply>,
    }

    impl Harness {
        fn new(batch_size: usize, interval: Duration, max_pending: usize) -> Self {
            let app = Arc::new(
                LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
                    .try_build()
                    .expect("start local system"),
            );
            let shutdown = app.shutdown_handle();
            let service = app
                .register_split_service::<Batcher, BatcherEvent, BatcherRequest, Infallible>(
                    Batcher::new(batch_size, interval, max_pending),
                    32,
                )
                .expect("register batcher");
            Self {
                app,
                shutdown,
                service,
            }
        }

        fn stats(&self) -> BatcherStats {
            match self
                .app
                .call_blocking_request(self.service.requests, BatcherRequest::Stats, CALL_TIMEOUT)
                .expect("stats admission")
            {
                CallOutcome::Replied(BatcherReply::Stats(stats)) => stats,
                other => panic!("unexpected stats outcome: {other:?}"),
            }
        }

        fn wait_for(&self, predicate: impl Fn(BatcherStats) -> bool) -> BatcherStats {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let stats = self.stats();
                if predicate(stats) {
                    return stats;
                }
                assert!(Instant::now() < deadline, "condition timed out: {stats:?}");
                thread::sleep(Duration::from_millis(1));
            }
        }

        fn spawn_call(
            &self,
            item: u64,
            timeout: Duration,
        ) -> thread::JoinHandle<CallOutcome<BatcherReply>> {
            let app = Arc::clone(&self.app);
            let requests: Requests = self.service.requests;
            thread::spawn(move || {
                app.call_blocking_request(requests, BatcherRequest::Submit(item), timeout)
                    .expect("call admission")
            })
        }

        fn shutdown(self) {
            let terminal = self
                .shutdown
                .request_and_wait_report(SHUTDOWN_TIMEOUT)
                .expect("observe shutdown");
            drop(self.app);
            terminal.ensure_clean().expect("clean shutdown");
        }
    }

    #[test]
    fn size_flush_replies_every_waiter_with_the_batch_total() {
        let harness = Harness::new(3, Duration::from_secs(5), 3);
        let one = harness.spawn_call(2, CALL_TIMEOUT);
        let two = harness.spawn_call(3, CALL_TIMEOUT);
        harness.wait_for(|stats| stats.waiters == 2);
        let three = harness.spawn_call(5, CALL_TIMEOUT);

        for caller in [one, two, three] {
            assert_eq!(
                caller.join().expect("caller joins"),
                CallOutcome::Replied(BatcherReply::Batched(10))
            );
        }
        let stats = harness.stats();
        assert_eq!(stats.size_flushes, 1);
        assert_eq!(stats.timer_flushes, 0);
        assert_eq!(stats.waiters, 0);
        assert_eq!(stats.items, 0);
        harness.shutdown();
    }

    #[test]
    fn timer_flush_replies_a_partial_batch() {
        let harness = Harness::new(3, Duration::from_millis(20), 3);
        let caller = harness.spawn_call(7, CALL_TIMEOUT);
        assert_eq!(
            caller.join().expect("caller joins"),
            CallOutcome::Replied(BatcherReply::Batched(7))
        );
        let stats = harness.stats();
        assert_eq!(stats.size_flushes, 0);
        assert_eq!(stats.timer_flushes, 1);
        assert_eq!(stats.current_generation, 2);
        harness.shutdown();
    }

    #[test]
    fn full_caller_gone_and_refill_settle_exactly_once() {
        let harness = Harness::new(8, Duration::from_secs(5), 1);
        let gone = harness.spawn_call(1, Duration::from_millis(20));
        assert_eq!(gone.join().expect("caller joins"), CallOutcome::Timeout);

        let live = harness.spawn_call(2, CALL_TIMEOUT);
        let admitted = harness.wait_for(|stats| stats.waiters == 1 && stats.reclaimed_callers == 1);
        assert_eq!(admitted.waiter_high_water, 1);

        assert_eq!(
            harness
                .app
                .call_blocking_request(
                    harness.service.requests,
                    BatcherRequest::Submit(99),
                    CALL_TIMEOUT,
                )
                .expect("full call admission"),
            CallOutcome::Replied(BatcherReply::Full)
        );
        assert_eq!(harness.stats().full_rejects, 1);

        harness
            .app
            .try_send_event(harness.service.events, BatcherEvent::Tick(1, Ok(())))
            .expect("manual flush tick");
        assert_eq!(
            live.join().expect("caller joins"),
            CallOutcome::Replied(BatcherReply::Batched(3))
        );
        let settled = harness.stats();
        assert_eq!(settled.waiters, 0);
        assert_eq!(settled.timer_flushes, 1);
        assert_eq!(settled.reclaimed_callers, 1);
        assert_eq!(settled.full_rejects, 1);

        let refilled = harness.spawn_call(5, CALL_TIMEOUT);
        harness.wait_for(|stats| stats.current_generation == 2 && stats.waiters == 1);
        harness
            .app
            .try_send_event(harness.service.events, BatcherEvent::Tick(2, Ok(())))
            .expect("post-full refill tick");
        assert_eq!(
            refilled.join().expect("refilled caller joins"),
            CallOutcome::Replied(BatcherReply::Batched(5))
        );
        let refilled_stats = harness.stats();
        assert_eq!(refilled_stats.waiters, 0);
        assert_eq!(refilled_stats.current_generation, 3);
        harness.shutdown();
    }

    #[test]
    fn stale_timer_cannot_flush_the_next_generation() {
        let harness = Harness::new(2, Duration::from_secs(5), 4);
        let one = harness.spawn_call(4, CALL_TIMEOUT);
        harness.wait_for(|stats| stats.waiters == 1);
        let two = harness.spawn_call(6, CALL_TIMEOUT);
        assert_eq!(
            one.join().expect("caller joins"),
            CallOutcome::Replied(BatcherReply::Batched(10))
        );
        assert_eq!(
            two.join().expect("caller joins"),
            CallOutcome::Replied(BatcherReply::Batched(10))
        );

        let next = harness.spawn_call(9, CALL_TIMEOUT);
        harness.wait_for(|stats| stats.current_generation == 2 && stats.waiters == 1);
        harness
            .app
            .try_send_event(
                harness.service.events,
                BatcherEvent::Tick(1, Err(CallError::TargetFull)),
            )
            .expect("stale failed tick admission");
        let stale = harness.wait_for(|stats| stats.stale_ticks == 1);
        assert_eq!(stale.waiters, 1);
        assert_eq!(stale.items, 1);
        assert_eq!(stale.timer_flushes, 0);
        assert_eq!(stale.timer_failures, 0);

        harness
            .app
            .try_send_event(harness.service.events, BatcherEvent::Tick(2, Ok(())))
            .expect("current tick admission");
        assert_eq!(
            next.join().expect("caller joins"),
            CallOutcome::Replied(BatcherReply::Batched(9))
        );
        harness.shutdown();
    }

    #[test]
    fn timer_failure_settles_every_waiter_and_refills_the_generation() {
        let harness = Harness::new(4, Duration::from_secs(5), 4);
        let one = harness.spawn_call(4, CALL_TIMEOUT);
        let two = harness.spawn_call(6, CALL_TIMEOUT);
        harness.wait_for(|stats| stats.waiters == 2);

        harness
            .app
            .try_send_event(
                harness.service.events,
                BatcherEvent::Tick(1, Err(CallError::TargetFull)),
            )
            .expect("failed tick admission");
        for caller in [one, two] {
            assert_eq!(
                caller.join().expect("caller joins"),
                CallOutcome::Replied(BatcherReply::TimerFailed(CallError::TargetFull))
            );
        }
        let failed = harness.stats();
        assert_eq!(failed.timer_failures, 1);
        assert_eq!(failed.current_generation, 2);
        assert_eq!(failed.waiters, 0);
        assert_eq!(failed.items, 0);

        let refilled = harness.spawn_call(9, CALL_TIMEOUT);
        harness.wait_for(|stats| stats.waiters == 1);
        harness
            .app
            .try_send_event(harness.service.events, BatcherEvent::Tick(2, Ok(())))
            .expect("refill tick admission");
        assert_eq!(
            refilled.join().expect("caller joins"),
            CallOutcome::Replied(BatcherReply::Batched(9))
        );
        harness.shutdown();
    }

    #[test]
    fn shutdown_observation_is_bounded() {
        let harness = Harness::new(2, Duration::from_secs(5), 2);
        let (sent, received) = mpsc::channel();
        let shutdown = harness.shutdown.clone();
        thread::spawn(move || {
            sent.send(shutdown.request_and_wait_report(SHUTDOWN_TIMEOUT))
                .expect("send shutdown outcome");
        });
        let terminal = received
            .recv_timeout(SHUTDOWN_TIMEOUT)
            .expect("bounded shutdown observation")
            .expect("shutdown succeeds");
        drop(harness.app);
        terminal.ensure_clean().expect("clean shutdown");
    }

    #[test]
    fn host_control_errors_remain_exhaustive_and_distinct() {
        let mut report = Report::default();
        for error in [
            ThreadedRuntimeError::CommandFull,
            ThreadedRuntimeError::WorkerStopped,
            ThreadedRuntimeError::HostWaitTimeout,
            ThreadedRuntimeError::WorkerUnresponsive,
            ThreadedRuntimeError::UnknownShard(ShardId::new(99)),
            ThreadedRuntimeError::DriverShutdownFailed,
            ThreadedRuntimeError::DriverParkFailed,
        ] {
            record_host_error(&mut report, error);
        }
        assert_eq!(report.host_command_full, 1);
        assert_eq!(report.host_worker_stopped, 1);
        assert_eq!(report.host_wait_timeout, 1);
        assert_eq!(report.host_worker_unresponsive, 1);
        assert_eq!(report.host_unknown_shard, 1);
        assert_eq!(report.host_driver_shutdown_failed, 1);
        assert_eq!(report.host_driver_park_failed, 1);
    }
}
