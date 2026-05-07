use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, ThreadedSendObservedError,
    ThreadedTrySendError, sleep,
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
    pending: u32,
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
                let was_idle = self.pending == 0;
                self.pending += 1;
                if was_idle {
                    sleep(self.work).reply(StoreMsg::Tick)
                } else {
                    noop()
                }
            }
            StoreMsg::Tick(reply) => {
                if reply.is_err() {
                    return stop();
                }
                self.processed += 1;
                self.pending -= 1;
                if self.is_done() {
                    stop()
                } else if self.pending > 0 {
                    sleep(self.work).reply(StoreMsg::Tick)
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
            Some(n) => self.pending == 0 && self.processed >= n,
            None => false,
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let mut stores = Vec::with_capacity(SHARDS as usize);
    for _ in 0..SHARDS {
        stores.push(
            runtime
                .register_with_capacity::<_, Infallible>(
                    Store {
                        work: Duration::from_millis(PER_WRITE_MS),
                        pending: 0,
                        processed: 0,
                        expected: None,
                    },
                    SHARD_MAILBOX,
                )
                .map_err(|e| anyhow::anyhow!("register store: {e:?}"))?,
        );
    }

    let counters: Vec<_> = (0..SHARDS)
        .map(|_| {
            (
                Arc::new(AtomicU32::new(0)),
                Arc::new(AtomicU32::new(0)),
                Arc::new(AtomicU32::new(0)),
            )
        })
        .collect();

    let burst = |shard_idx: usize, n: u32| -> anyhow::Result<()> {
        let (admitted, full, observed) = (
            Arc::clone(&counters[shard_idx].0),
            Arc::clone(&counters[shard_idx].1),
            Arc::clone(&counters[shard_idx].2),
        );
        for _ in 0..n {
            let admitted_obs = Arc::clone(&admitted);
            let full_obs = Arc::clone(&full);
            let observed_obs = Arc::clone(&observed);
            let observer = move |outcome: Result<(), ThreadedSendObservedError>| {
                match outcome {
                    Ok(()) => admitted_obs.fetch_add(1, Ordering::Release),
                    Err(_) => full_obs.fetch_add(1, Ordering::Release),
                };
                observed_obs.fetch_add(1, Ordering::Release);
            };
            match runtime.try_send_and_observe_with(stores[shard_idx], StoreMsg::Set, observer) {
                Ok(()) => {}
                Err(ThreadedTrySendError::IngressFull) => {
                    full.fetch_add(1, Ordering::Release);
                    observed.fetch_add(1, Ordering::Release);
                }
                Err(e) => return Err(anyhow::anyhow!("burst: {e}")),
            }
        }
        Ok(())
    };

    burst(0, HOT_WRITES)?;
    for shard in 1..SHARDS as usize {
        burst(shard, COLD_WRITES_PER_SHARD)?;
    }

    // Wait for every observer to fire.
    let total_writes = HOT_WRITES + (SHARDS - 1) * COLD_WRITES_PER_SHARD;
    let total_observed: u32 = counters
        .iter()
        .map(|(_, _, o)| o.load(Ordering::Acquire))
        .sum();
    let deadline = Instant::now() + Duration::from_secs(2);
    while total_observed < total_writes
        && Instant::now() < deadline
        && counters
            .iter()
            .map(|(_, _, o)| o.load(Ordering::Acquire))
            .sum::<u32>()
            < total_writes
    {
        std::thread::yield_now();
    }

    // Register stop-watchers FIRST so a fast-draining store cannot
    // stop before the waiter is in place.
    let waiters: Vec<_> = stores
        .iter()
        .map(|s| runtime.observe_isolate_complete(*s))
        .collect();

    // Drain each store. The mailbox may still hold queued Sets, so
    // retry until accepted.
    for (idx, s) in stores.iter().enumerate() {
        let admitted_n = counters[idx].0.load(Ordering::Acquire);
        let close_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match runtime.send_and_observe(*s, StoreMsg::Drain(admitted_n)) {
                Ok(()) => break,
                Err(ThreadedSendObservedError::MailboxFull)
                | Err(ThreadedSendObservedError::IngressFull) => {
                    if Instant::now() >= close_deadline {
                        return Err(anyhow::anyhow!("could not deliver Drain"));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => return Err(anyhow::anyhow!("drain send: {e}")),
            }
        }
    }
    for w in waiters {
        w.wait(Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("store stop: {e:?}"))?;
    }

    let hot_admitted = counters[0].0.load(Ordering::Acquire);
    let hot_rejected = counters[0].1.load(Ordering::Acquire);
    let mut cold_admitted = 0u32;
    let mut cold_rejected = 0u32;
    for (a, f, _) in &counters[1..] {
        cold_admitted += a.load(Ordering::Acquire);
        cold_rejected += f.load(Ordering::Acquire);
    }

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(Report {
        hot_admitted,
        hot_rejected,
        cold_admitted,
        cold_rejected,
        exit_clean: true,
    })
}
