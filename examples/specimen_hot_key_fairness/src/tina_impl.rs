use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, FairnessReport, HostBurstOutcomes, SingleCallGate, SleepReply,
    ThreadedRuntime, sleep, stable_trace_hash,
};

use crate::{COLD_WRITES_PER_SHARD, HOT_WRITES, PER_WRITE_MS, Report, SHARD_MAILBOX, SHARDS};

#[derive(Debug)]
enum StoreMsg {
    Set,
    Tick(SleepReply),
    /// Host-provided expected admission count. Store stops when
    /// `processed >= expected && pending == 0`. The host computes
    /// the count from its own per-send observers, so the figure is
    /// authoritative and not racing against queued Sets.
    Drain(u32),
}

struct Store {
    work: Duration,
    /// The single-call gate invariant names the "one Tick in flight, plus N
    /// queued" invariant.
    gate: SingleCallGate,
    processed: u32,
    expected: Option<u32>,
}

#[tina_runtime::isolate(message = StoreMsg)]
impl Store {
    fn handle(
        &mut self,
        msg: StoreMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            StoreMsg::Set => {
                if self.gate.submit() {
                    sleep(self.work).then(StoreMsg::Tick)
                } else {
                    noop()
                }
            }
            StoreMsg::Tick(reply) => {
                if reply.is_err() {
                    self.gate.cancel_in_flight();
                    return stop();
                }
                self.processed += 1;
                let more = self.gate.complete();
                if self.is_done() {
                    stop()
                } else if more {
                    sleep(self.work).then(StoreMsg::Tick)
                } else {
                    noop()
                }
            }
            StoreMsg::Drain(expected) => {
                self.expected = Some(expected);
                if self.is_done() { stop() } else { noop() }
            }
        }
    }
}

impl Store {
    fn is_done(&self) -> bool {
        match self.expected {
            Some(n) => self.gate.is_idle() && self.processed >= n,
            None => false,
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();

    let mut stores = Vec::with_capacity(SHARDS as usize);
    for _ in 0..SHARDS {
        stores.push(
            runtime
                .register_with_capacity::<_, Infallible>(
                    Store {
                        work: Duration::from_millis(PER_WRITE_MS),
                        gate: SingleCallGate::new(),
                        processed: 0,
                        expected: None,
                    },
                    SHARD_MAILBOX,
                )
                .map_err(|e| anyhow::anyhow!("register store: {e:?}"))?,
        );
    }

    let outcomes: Vec<HostBurstOutcomes> = (0..SHARDS).map(|_| HostBurstOutcomes::new()).collect();

    for _ in 0..HOT_WRITES {
        let _ = runtime.try_send_outcome(stores[0], StoreMsg::Set, &outcomes[0]);
    }
    for shard in 1..SHARDS as usize {
        for _ in 0..COLD_WRITES_PER_SHARD {
            let _ = runtime.try_send_outcome(stores[shard], StoreMsg::Set, &outcomes[shard]);
        }
    }

    for o in &outcomes {
        o.wait_complete(Duration::from_secs(2))
            .map_err(|e| anyhow::anyhow!("burst observers: {e}"))?;
    }

    // Register stop-watchers FIRST so a fast-draining store cannot
    // stop before the waiter is in place.
    let waiters: Vec<_> = stores
        .iter()
        .map(|s| runtime.observe_isolate_complete(*s))
        .collect::<Result<_, _>>()?;

    // Drain each store with the host-counted admitted total. The
    // mailbox may still hold queued Sets, so retry until accepted.
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    let backoff = Duration::from_millis(2);
    for (idx, s) in stores.iter().enumerate() {
        let admitted_n = outcomes[idx].snapshot().admitted;
        runtime
            .send_observed_until(*s, drain_deadline, backoff, || StoreMsg::Drain(admitted_n))
            .map_err(|e| anyhow::anyhow!("drain send: {e:?}"))?;
    }
    for w in waiters {
        w.wait(Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("store stop: {e:?}"))?;
    }

    let hot_snap = outcomes[0].snapshot();
    let hot_admitted = hot_snap.admitted;
    let hot_rejected = hot_snap.mailbox_full + hot_snap.ingress_full;
    let mut cold_admitted = 0u32;
    let mut cold_rejected = 0u32;
    for o in &outcomes[1..] {
        let s = o.snapshot();
        cold_admitted += s.admitted;
        cold_rejected += s.mailbox_full + s.ingress_full;
    }

    let trace = runtime.trace();
    let fairness = FairnessReport::from_events(trace.events().iter());
    let hot_isolate = stores[0].isolate();
    let hot_turns = fairness.turns(hot_isolate);
    let cold_turns: Vec<u64> = stores[1..]
        .iter()
        .map(|store| fairness.turns(store.isolate()))
        .collect();
    let cold_min_turns = cold_turns.iter().copied().min().unwrap_or(0);
    let cold_expected_turns = outcomes[1..]
        .iter()
        .map(|outcome| u64::from(outcome.snapshot().admitted) * 2 + 1)
        .collect::<Vec<_>>();
    let cold_min_expected_turns = cold_expected_turns.iter().copied().min().unwrap_or(0);
    let max_cold_progress_deficit_turns = cold_expected_turns
        .iter()
        .zip(cold_turns.iter())
        .map(|(expected, actual)| expected.saturating_sub(*actual))
        .max()
        .unwrap_or(0);
    let progress_gaps = stores[1..]
        .iter()
        .map(|store| fairness.progress_gap(store.isolate(), hot_isolate, None))
        .collect::<Vec<_>>();
    let max_progress_gap_turns = progress_gaps
        .iter()
        .map(|lag| lag.observed)
        .max()
        .unwrap_or(0);
    let fairness_line = format!(
        "{} {}",
        fairness,
        progress_gaps
            .iter()
            .map(|lag| lag.summary_line())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let trace_hash = stable_trace_hash(trace.events().iter());

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(Report {
        hot_admitted,
        hot_rejected,
        cold_admitted,
        cold_rejected,
        hot_turns,
        cold_min_turns,
        cold_min_expected_turns,
        max_cold_progress_deficit_turns,
        max_progress_gap_turns,
        trace_hash,
        fairness_line,
        exit_clean: true,
    })
}
