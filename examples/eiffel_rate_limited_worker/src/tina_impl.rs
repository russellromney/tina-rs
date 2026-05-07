//! Tina side. The worker is a single isolate with a bounded mailbox.
//! Rate limiting is the worker's own state machine: each `Submit`
//! either kicks off `sleep(RATE_WINDOW)` or buffers as `pending` until
//! the in-flight Tick lands.
//!
//! The producer fires non-blocking `try_send_and_observe_with(...)` so
//! the worker thread drains the whole command burst before stepping
//! the isolate. That makes the bounded mailbox visible: jobs past the
//! cap come back as `MailboxFull` through the per-send observer
//! callback. `IngressFull` from the command queue is rolled into the
//! same "full" bucket — both are caller-visible overload at submit.
//!
//! What this teaches:
//!
//! - **Bounded mailbox is the data plane.** No internal `VecDeque`
//!   for queued submits; the runtime mailbox holds them.
//! - **Rate window via timer continuation.** `sleep(...).reply(Tick)`
//!   is one trace event per processed job. No hidden interval timer.
//! - **One in-flight Tick at a time.** A `pending` counter inside
//!   the isolate serializes submits over the rate window — without
//!   it, multiple `sleep` continuations would fire in parallel and
//!   the rate limit would not exist.
//! - **End-of-burst as a Tina message.** The host signals "no more
//!   submits" via [`WorkerMsg::BurstClosed`] (sent through the same
//!   bounded mailbox) so the worker stops the moment its `processed`
//!   count catches up — no `Arc<AtomicU32>` side channel.
//! - **Final value via `stop_with`.** The host reads the worker's
//!   processed count through `runtime.observe_result::<Report>`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, HostBurstOutcomes, SingleCallGate, SleepReply, ThreadedRuntime,
    sleep,
};

use crate::{BURST_JOBS, QUEUE_CAPACITY, RATE_WINDOW_MS, Report};

#[derive(Debug)]
enum WorkerMsg {
    /// One job submission from the host. The `u32` is the job index;
    /// the worker only logs it (and we deliberately bind it in the
    /// handler so the payload stays a real lesson, not a unit hole).
    Submit(u32),
    /// Sleep continuation marking "this job is done." Carries
    /// `SleepReply` so the canonical reply alias is visible at the
    /// reply site; the handler pattern-matches it.
    Tick(SleepReply),
    /// Host has finished bursting; the value names the final admitted
    /// count so the worker can stop the moment its `processed` count
    /// catches up. Sent through the same bounded mailbox the submits
    /// went through.
    BurstClosed(u32),
}

struct Worker {
    rate_window: Duration,
    report: Report,
    processed: u32,
    /// Single-in-flight timer guard. Bounded by the mailbox cap — the
    /// host stops sending past [`QUEUE_CAPACITY`].
    ///
    /// Phase-062 Rock 5: the `pending` / `was_idle` invariant is named
    /// by [`tina_runtime::SingleCallGate`] instead of being repeated
    /// inline.
    gate: SingleCallGate,
    /// `Some(n)` after [`WorkerMsg::BurstClosed`] arrives, naming the
    /// final admitted count the worker should stop at. `None` while
    /// the host is still bursting.
    expected: Option<u32>,
    /// Last job index the worker processed. Tracked so `Submit(u32)`
    /// is bound and read deliberately rather than ignored.
    last_index: Option<u32>,
}

#[tina_runtime::isolate(message = WorkerMsg)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Submit(index) => {
                self.report.jobs_admitted += 1;
                self.last_index = Some(index);
                // SingleCallGate names the "one in-flight Tick at a
                // time" invariant. `submit()` returns true on the
                // very first piece of work and false while a Tick is
                // still racing the rate window.
                if self.gate.submit() {
                    sleep(self.rate_window).reply(WorkerMsg::Tick)
                } else {
                    noop()
                }
            }
            WorkerMsg::Tick(reply) => {
                // The sleep is plain time. If it was cancelled (e.g.,
                // runtime shutdown), bail out cleanly rather than
                // pretend a job finished.
                if reply.is_err() {
                    self.report.exit_clean = false;
                    return stop_with(self.report);
                }
                self.processed += 1;
                self.report.jobs_processed = self.processed;
                let more_work = self.gate.complete();
                if self.is_done() {
                    self.report.exit_clean = true;
                    stop_with(self.report)
                } else if more_work {
                    sleep(self.rate_window).reply(WorkerMsg::Tick)
                } else {
                    noop()
                }
            }
            WorkerMsg::BurstClosed(admitted) => {
                self.expected = Some(admitted);
                if self.is_done() {
                    self.report.exit_clean = true;
                    stop_with(self.report)
                } else {
                    noop()
                }
            }
        }
    }
}

impl Worker {
    fn is_done(&self) -> bool {
        match self.expected {
            Some(expected) => self.gate.is_idle() && self.processed >= expected,
            None => false,
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let worker = Worker {
        rate_window: Duration::from_millis(RATE_WINDOW_MS),
        report: Report::default(),
        processed: 0,
        gate: SingleCallGate::new(),
        expected: None,
        last_index: None,
    };
    let worker_addr = runtime
        .register_with_capacity::<_, Infallible>(worker, QUEUE_CAPACITY)
        .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?;

    let waiter = runtime
        .observe_result::<Report, _, _>(worker_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    // Producer: non-blocking burst. The worker drains the whole
    // command burst before stepping the isolate, so the mailbox fills
    // up to `QUEUE_CAPACITY` and every send past that surfaces
    // `MailboxFull` through the per-send observer the helper installs.
    //
    // Phase-062 Rock 3: `try_send_outcome` + `HostBurstOutcomes`
    // replace the hand-rolled per-send closure / atomics / observed
    // barrier. The accumulator preserves every truth-typed outcome
    // (admitted, mailbox_full, mailbox_closed, ingress_full,
    // worker_stopped); none of them are collapsed.
    let outcomes = HostBurstOutcomes::new();
    for n in 0..BURST_JOBS {
        let _ = runtime.try_send_outcome(worker_addr, WorkerMsg::Submit(n), &outcomes);
    }
    outcomes
        .wait_complete(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("host burst observers did not fire: {e}"))?;
    let burst = outcomes.snapshot();
    let admitted_n = burst.admitted;

    // Tell the worker the burst is closed. The mailbox might still
    // hold up to `QUEUE_CAPACITY` admitted Submits, draining at one
    // per `RATE_WINDOW`, so retry until a slot opens. Worst-case wait
    // is `QUEUE_CAPACITY * RATE_WINDOW`, plus jitter.
    //
    // Phase-062 Rock 4: `send_observed_until` is the BurstClosed-style
    // helper. It retries on Full/IngressFull until the deadline and
    // returns typed Closed / WorkerStopped / Timeout. No hidden queue;
    // the control message rides the same bounded data mailbox.
    runtime
        .send_observed_until(
            worker_addr,
            Instant::now() + Duration::from_secs(2),
            Duration::from_millis(2),
            || WorkerMsg::BurstClosed(admitted_n),
        )
        .map_err(|e| anyhow::anyhow!("could not deliver BurstClosed: {e}"))?;

    let report = waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("worker did not finish: {e:?}"))?;

    let final_report = Report {
        jobs_admitted: admitted_n,
        // The producer's "full" bucket is visible overload at submit:
        // mailbox cap reached *or* worker ingress queue at cap. Both
        // are honest backpressure the host saw before the worker drained
        // the message. `mailbox_closed` and `worker_stopped` would
        // surface here too if the worker had stopped mid-burst.
        jobs_full: burst.mailbox_full + burst.ingress_full,
        jobs_processed: report.jobs_processed,
        exit_clean: report.exit_clean,
    };

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(final_report)
}
