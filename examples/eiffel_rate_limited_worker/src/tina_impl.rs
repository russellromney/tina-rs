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
//! - **Final value via `stop_with`.** The host reads the worker's
//!   processed count through `runtime.observe_result::<Report>`.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, ThreadedSendObservedError,
    ThreadedTrySendError, sleep,
};

use crate::{BURST_JOBS, QUEUE_CAPACITY, RATE_WINDOW_MS, Report};

#[derive(Debug)]
enum WorkerMsg {
    /// One job submission from the host.
    Submit(u32),
    /// Sleep continuation marking "this job is done."
    Tick(SleepReply),
}

struct Worker {
    /// Host writes the final admitted count here once the burst is
    /// done. Worker reads it on every Tick to decide whether to stop.
    expected_total: Arc<AtomicU32>,
    rate_window: Duration,
    report: Report,
    processed: u32,
    /// Submits accepted by the worker but not yet processed. Bounded
    /// by the mailbox cap — the host stops sending past
    /// [`QUEUE_CAPACITY`].
    pending: u32,
}

#[tina_runtime::isolate(message = WorkerMsg)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Submit(_) => {
                self.report.jobs_admitted += 1;
                let was_idle = self.pending == 0;
                self.pending += 1;
                // One in-flight Tick at a time. Without this guard,
                // every Submit would schedule its own sleep and the
                // rate limit would not exist.
                if was_idle {
                    sleep(self.rate_window).reply(WorkerMsg::Tick)
                } else {
                    noop()
                }
            }
            WorkerMsg::Tick(_) => {
                self.processed += 1;
                self.pending -= 1;
                self.report.jobs_processed = self.processed;
                let expected = self.expected_total.load(Ordering::Acquire);
                if expected > 0 && self.processed >= expected {
                    self.report.exit_clean = true;
                    stop_with(self.report)
                } else if self.pending > 0 {
                    sleep(self.rate_window).reply(WorkerMsg::Tick)
                } else {
                    noop()
                }
            }
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let expected_total = Arc::new(AtomicU32::new(0));
    let worker = Worker {
        expected_total: Arc::clone(&expected_total),
        rate_window: Duration::from_millis(RATE_WINDOW_MS),
        report: Report::default(),
        processed: 0,
        pending: 0,
    };
    let worker_addr = runtime
        .register_with_capacity::<_, Infallible>(worker, QUEUE_CAPACITY)
        .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?;

    let waiter = runtime
        .observe_result::<Report, _, _>(worker_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    // Producer: non-blocking burst. Each `try_send_and_observe_with`
    // queues a command on the worker. The worker drains the whole
    // burst before stepping the isolate, so the mailbox fills up to
    // `QUEUE_CAPACITY` and every send past that surfaces
    // `MailboxFull` through the observer callback.
    let admitted = Arc::new(AtomicU32::new(0));
    let mailbox_full = Arc::new(AtomicU32::new(0));
    let observed = Arc::new(AtomicU32::new(0));
    let mut ingress_full = 0u32;

    for n in 0..BURST_JOBS {
        let admitted_obs = Arc::clone(&admitted);
        let mailbox_full_obs = Arc::clone(&mailbox_full);
        let observed_obs = Arc::clone(&observed);
        let observer = move |outcome: Result<(), ThreadedSendObservedError>| {
            match outcome {
                Ok(()) => {
                    admitted_obs.fetch_add(1, Ordering::Release);
                }
                Err(_) => {
                    mailbox_full_obs.fetch_add(1, Ordering::Release);
                }
            }
            observed_obs.fetch_add(1, Ordering::Release);
        };
        match runtime.try_send_and_observe_with(worker_addr, WorkerMsg::Submit(n), observer) {
            Ok(()) => {}
            Err(ThreadedTrySendError::IngressFull) => {
                ingress_full += 1;
                observed.fetch_add(1, Ordering::Release);
            }
            Err(e) => return Err(anyhow::anyhow!("try_send_and_observe_with: {e}")),
        }
    }

    // Wait for every observer to fire. Each one runs on the worker
    // thread; the burst is small so this is fast.
    let deadline = Instant::now() + Duration::from_secs(2);
    while observed.load(Ordering::Acquire) < BURST_JOBS && Instant::now() < deadline {
        std::thread::yield_now();
    }
    if observed.load(Ordering::Acquire) < BURST_JOBS {
        return Err(anyhow::anyhow!(
            "observers did not all fire within 2s ({}/{})",
            observed.load(Ordering::Acquire),
            BURST_JOBS,
        ));
    }

    let admitted_n = admitted.load(Ordering::Acquire);
    expected_total.store(admitted_n, Ordering::Release);

    let report = waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("worker did not finish: {e:?}"))?;

    let final_report = Report {
        jobs_admitted: admitted_n,
        jobs_full: mailbox_full.load(Ordering::Acquire) + ingress_full,
        jobs_processed: report.jobs_processed,
        exit_clean: report.exit_clean,
    };

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(final_report)
}
