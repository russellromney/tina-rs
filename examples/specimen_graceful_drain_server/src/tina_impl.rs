//! Tina side. The worker isolate uses the same bounded mailbox for
//! jobs and the shutdown signal — `Drain` is just another message in
//! `WorkerMsg`. After `Drain` arrives, the worker captures the
//! observed admitted count as the "expected" total and continues to
//! drain. When the gate is idle and `processed >= expected`, the
//! worker `stop_with(report)` and the host's
//! `observe_result::<Report>` waiter resolves.
//!
//! What this teaches:
//!
//! - Shutdown is a message. No `select!`, no `oneshot`, no second
//!   channel; the same mailbox carries jobs and shutdown.
//! - "In flight" and "pending in queue" are one number, named by
//!   `SingleCallGate`. Drain truth is local: the
//!   gate becomes idle.
//! - Final `Report` reaches the host via `observe_result`. No
//!   `Arc<Mutex>`, no mpsc.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, HostBurstOutcomes, SingleCallGate, SleepReply, ThreadedRuntime,
    sleep,
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
    /// The single-call gate invariant names the "one Tick in flight, plus N
    /// queued" invariant.
    gate: SingleCallGate,
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
                if self.gate.submit() {
                    sleep(self.work).then(WorkerMsg::Tick)
                } else {
                    noop()
                }
            }
            WorkerMsg::Tick(reply) => {
                if reply.is_err() {
                    self.gate.cancel_in_flight();
                    self.report.exit_clean = false;
                    return stop_with(self.report);
                }
                self.processed += 1;
                let more = self.gate.complete();
                self.report.items_processed = self.processed;
                if self.drained_and_done() {
                    self.report.exit_clean = true;
                    stop_with(self.report)
                } else if more {
                    sleep(self.work).then(WorkerMsg::Tick)
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
            Some(expected) => self.gate.is_idle() && self.processed >= expected,
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

    let worker_addr = runtime
        .register_with_capacity::<_, Infallible>(
            Worker {
                work: Duration::from_millis(JOB_WORK_MS),
                gate: SingleCallGate::new(),
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

    // Producer: non-blocking burst with `try_send_outcome` /
    // `HostBurstOutcomes` (the host burst outcome helpers).
    let outcomes = HostBurstOutcomes::new();
    for n in 0..BURST_JOBS {
        let _ = runtime.try_send_outcome(worker_addr, WorkerMsg::Submit(n), &outcomes);
    }
    outcomes
        .wait_complete(Duration::from_secs(2))
        .map_err(|e| anyhow::anyhow!("burst observers: {e}"))?;
    let snap = outcomes.snapshot();

    // Drain rides the same bounded mailbox; `send_observed_until`
    // retries on `MailboxFull` / `IngressFull` up to the deadline.
    let close_deadline = Instant::now() + Duration::from_secs(2);
    runtime
        .send_observed_until(
            worker_addr,
            close_deadline,
            Duration::from_millis(2),
            || WorkerMsg::Drain,
        )
        .map_err(|e| anyhow::anyhow!("Drain send: {e:?}"))?;

    let report = waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("worker did not finish: {e:?}"))?;

    let final_report = Report {
        items_admitted: snap.admitted,
        items_full: snap.mailbox_full + snap.ingress_full,
        items_processed: report.items_processed,
        shutdown_observed: report.shutdown_observed,
        exit_clean: report.exit_clean,
    };

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;
    Ok(final_report)
}
