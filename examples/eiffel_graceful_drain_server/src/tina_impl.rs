//! Tina side. The worker isolate uses the same bounded mailbox for
//! jobs and the shutdown signal — `Drain` is just another message in
//! `WorkerMsg`. After `Drain` arrives, the worker captures the
//! observed admitted count as the "expected" total and continues to
//! drain. When `pending == 0 && processed >= expected`, the worker
//! `stop_with(report)` and the host's `observe_result::<Report>`
//! waiter resolves.
//!
//! What this teaches:
//!
//! - Shutdown is a message. No `select!`, no `oneshot`, no second
//!   channel; the same mailbox carries jobs and shutdown.
//! - "In flight" and "pending in queue" are one number: `pending`,
//!   bounded by the mailbox cap. Drain truth is local: `pending`
//!   reaches 0.
//! - Final `Report` reaches the host via `observe_result`. No
//!   `Arc<Mutex>`, no mpsc.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, SleepReply, ThreadedRuntime, ThreadedSendObservedError,
    ThreadedTrySendError, sleep,
};

use crate::{BURST_JOBS, JOB_WORK_MS, QUEUE_CAPACITY, Report};

#[derive(Debug)]
enum WorkerMsg {
    /// One job submission.
    Submit(u32),
    /// One sleep continuation; finishes a job.
    Tick(SleepReply),
    /// Host has signalled shutdown. Drain in-flight + queued, then
    /// stop with the final report.
    Drain,
}

struct Worker {
    work: Duration,
    pending: u32,
    processed: u32,
    /// `Some(n)` after `Drain` — the admitted count to drain to.
    expected: Option<u32>,
    /// Last submitted job index, kept so the `Submit(u32)` payload is
    /// read deliberately.
    last_index: Option<u32>,
    report: Report,
}

#[tina_runtime::isolate(message = WorkerMsg)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Submit(index) => {
                self.last_index = Some(index);
                self.report.items_admitted += 1;
                let was_idle = self.pending == 0;
                self.pending += 1;
                if was_idle {
                    sleep(self.work).reply(WorkerMsg::Tick)
                } else {
                    noop()
                }
            }
            WorkerMsg::Tick(reply) => {
                if reply.is_err() {
                    self.report.exit_clean = false;
                    return stop_with(self.report);
                }
                self.processed += 1;
                self.pending -= 1;
                self.report.items_processed = self.processed;
                if self.drained_and_done() {
                    self.report.exit_clean = true;
                    stop_with(self.report)
                } else if self.pending > 0 {
                    sleep(self.work).reply(WorkerMsg::Tick)
                } else {
                    noop()
                }
            }
            WorkerMsg::Drain => {
                self.report.shutdown_observed = true;
                self.expected = Some(self.report.items_admitted);
                if self.drained_and_done() {
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
    fn drained_and_done(&self) -> bool {
        match self.expected {
            Some(expected) => self.pending == 0 && self.processed >= expected,
            None => false,
        }
    }
}

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let worker_addr = runtime
        .register_with_capacity::<_, Infallible>(
            Worker {
                work: Duration::from_millis(JOB_WORK_MS),
                pending: 0,
                processed: 0,
                expected: None,
                last_index: None,
                report: Report::default(),
            },
            QUEUE_CAPACITY,
        )
        .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?;

    let waiter = runtime
        .observe_result::<Report, _, _>(worker_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    // Producer: same shape as eiffel_rate_limited_worker. Non-blocking
    // burst with per-send observer to count admit / full.
    let admitted = Arc::new(AtomicU32::new(0));
    let full = Arc::new(AtomicU32::new(0));
    let observed = Arc::new(AtomicU32::new(0));
    let mut ingress_full = 0u32;

    for n in 0..BURST_JOBS {
        let admitted_obs = Arc::clone(&admitted);
        let full_obs = Arc::clone(&full);
        let observed_obs = Arc::clone(&observed);
        let observer = move |outcome: Result<(), ThreadedSendObservedError>| {
            match outcome {
                Ok(()) => {
                    admitted_obs.fetch_add(1, Ordering::Release);
                }
                Err(_) => {
                    full_obs.fetch_add(1, Ordering::Release);
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

    // Wait for observers.
    let deadline = Instant::now() + Duration::from_secs(2);
    while observed.load(Ordering::Acquire) < BURST_JOBS && Instant::now() < deadline {
        std::thread::yield_now();
    }
    if observed.load(Ordering::Acquire) < BURST_JOBS {
        return Err(anyhow::anyhow!("observers did not all fire within 2s"));
    }

    // Drain message goes through the same mailbox. The mailbox may
    // still have admitted Submits queued (worker drains them at one
    // per JOB_WORK_MS), so retry until accepted.
    let close_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match runtime.send_and_observe(worker_addr, WorkerMsg::Drain) {
            Ok(()) => break,
            Err(ThreadedSendObservedError::MailboxFull)
            | Err(ThreadedSendObservedError::IngressFull) => {
                if Instant::now() >= close_deadline {
                    return Err(anyhow::anyhow!("could not deliver Drain within 2s"));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(anyhow::anyhow!("Drain send: {e}")),
        }
    }

    let report = waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("worker did not finish: {e:?}"))?;

    let final_report = Report {
        items_admitted: admitted.load(Ordering::Acquire),
        items_full: full.load(Ordering::Acquire) + ingress_full,
        items_processed: report.items_processed,
        shutdown_observed: report.shutdown_observed,
        exit_clean: report.exit_clean,
    };

    if let Ok(rt) = Arc::try_unwrap(runtime) {
        let _ = rt.shutdown();
    }
    Ok(final_report)
}
