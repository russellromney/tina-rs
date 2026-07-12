//! Tina side. The worker is a single isolate with a bounded mailbox.
//! Pacing is a `tina_runtime::RateLimit` token bucket: on each "ready to
//! work" moment the worker asks `try_admit((), ctx.now())`. `Admitted`
//! means process one job now; `RateLimited { retry_after }` means sleep
//! exactly that long and ask again. The rate window is no longer a
//! hand-rolled `sleep(RATE_WINDOW)` constant — it falls out of the
//! limiter's deterministic `retry_after`.
//!
//! The producer fires non-blocking `try_send_outcome(...)` against a
//! shared `HostBurstOutcomes` (the host burst outcome helpers). The worker
//! drains the whole command burst before stepping the isolate, so
//! jobs past the cap come back as `MailboxFull` /
//! `IngressFull` in the typed snapshot — both visible at submit.
//!
//! What this teaches:
//!
//! - **Bounded mailbox is the data plane.** No internal `VecDeque`
//!   for queued submits; the runtime mailbox holds them.
//! - **Pacing is a real admission policy.** `RateLimit<()>` with burst 1
//!   admits one job, then returns `RateLimited { retry_after }` until a
//!   token refills. The worker owns the wait (`sleep(retry_after)`); the
//!   limiter never sleeps for it. `ctx.now()` drives the bucket, so the
//!   path is replayable.
//! - **One in-flight pace timer at a time.** A `pacing` flag plus an
//!   explicit `pending` count serialize work over the rate window —
//!   without them, multiple `sleep` continuations would fire in parallel.
//! - **End-of-burst as a Tina message.** The host signals "no more
//!   submits" via `WorkerMsg::BurstClosed` (sent through the same
//!   bounded mailbox) so the worker stops the moment its `processed`
//!   count catches up — no `Arc<AtomicU32>` side channel.
//! - **Final value via `stop_with`.** The host reads the worker's
//!   processed count through `app.observe_result::<Report>`.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    AdmissionDecision, DefaultThreadedMailboxFactory, HostBurstOutcomes, LocalSystem, RateLimit,
    SleepReply, sleep,
};

use crate::{BURST_JOBS, QUEUE_CAPACITY, RATE_WINDOW_MS, Report};

#[derive(Debug)]
enum WorkerMsg {
    /// One job submission from the host. The `u32` is the job index;
    /// the worker only logs it (and we deliberately bind it in the
    /// handler so the payload stays a real lesson, not a unit hole).
    Submit(u32),
    /// Backoff continuation marking "the rate window elapsed, try the next
    /// job." Carries `SleepReply` so a cancelled timer (shutdown) is
    /// visible at the reply site.
    Tick(SleepReply),
    /// Host has finished bursting; the value names the final admitted
    /// count so the worker can stop the moment its `processed` count
    /// catches up. Sent through the same bounded mailbox the submits
    /// went through.
    BurstClosed(u32),
}

struct Worker {
    /// Pacing policy. `try_admit((), now)` is the rate gate; its
    /// `retry_after` is the worker's sleep budget. Single global key `()`.
    limiter: RateLimit<()>,
    report: Report,
    processed: u32,
    /// Jobs admitted into the worker but not yet processed.
    pending: u32,
    /// `true` while a backoff `Tick` timer is in flight. Serializes the
    /// pace loop to one outstanding timer.
    pacing: bool,
    /// `Some(n)` after [`WorkerMsg::BurstClosed`] arrives, naming the
    /// final admitted count the worker should stop at.
    expected: Option<u32>,
    /// Last job index the worker processed. Tracked so `Submit(u32)`
    /// is bound and read deliberately rather than ignored.
    last_index: Option<u32>,
}

#[tina_runtime::isolate(message = WorkerMsg)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Submit(index) => {
                self.report.jobs_admitted += 1;
                self.last_index = Some(index);
                self.pending += 1;
                // Kick the pace loop only if no timer is already in flight.
                if self.pacing { noop() } else { self.drive(ctx) }
            }
            WorkerMsg::Tick(reply) => {
                // The backoff sleep is plain time. If it was cancelled
                // (e.g., runtime shutdown), bail out cleanly.
                if reply.is_err() {
                    self.report.exit_clean = false;
                    return stop_with(self.report);
                }
                self.pacing = false;
                self.drive(ctx)
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
    /// Process as many pending jobs as the limiter currently allows, then
    /// either stop (done), schedule a backoff `Tick` (rate-limited), or go
    /// idle (no pending). With burst 1 the loop processes exactly one job
    /// per turn, so this paces one job per refill window.
    fn drive(&mut self, ctx: &mut Context<'_, SingleShard, ()>) -> Effect<Self> {
        loop {
            if self.pending == 0 {
                // Nothing to do right now. Stop if the burst is closed and
                // we've caught up; otherwise wait for the next Submit.
                return if self.is_done() {
                    self.report.exit_clean = true;
                    stop_with(self.report)
                } else {
                    noop()
                };
            }
            match self.limiter.try_admit(&(), ctx.now()) {
                AdmissionDecision::Admitted(_grant) => {
                    self.processed += 1;
                    self.pending -= 1;
                    self.report.jobs_processed = self.processed;
                    if self.is_done() {
                        self.report.exit_clean = true;
                        return stop_with(self.report);
                    }
                    // Loop: try the next job. Within one turn `ctx.now()` is
                    // fixed, so burst 1 means the next iteration is
                    // RateLimited and we fall through to the sleep.
                }
                AdmissionDecision::RateLimited { retry_after, .. } => {
                    self.pacing = true;
                    return sleep(retry_after).then(WorkerMsg::Tick);
                }
                // Single global key `()` in a 1-key table that is always
                // present, never closed — these arms are unreachable.
                AdmissionDecision::Full(_)
                | AdmissionDecision::Closed(_)
                | AdmissionDecision::Wait { .. }
                | AdmissionDecision::Degrade { .. }
                | AdmissionDecision::TimedOut(_) => {
                    self.report.exit_clean = false;
                    return stop_with(self.report);
                }
            }
        }
    }

    fn is_done(&self) -> bool {
        match self.expected {
            Some(expected) => self.pending == 0 && !self.pacing && self.processed >= expected,
            None => false,
        }
    }
}

/// Tokens per second that reproduce one job per [`RATE_WINDOW_MS`].
fn rate_per_sec() -> u64 {
    // 1 token per RATE_WINDOW_MS = 1000 / RATE_WINDOW_MS tokens/sec.
    (1_000 / RATE_WINDOW_MS).max(1)
}

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    let result = run_application(&app);
    let shutdown = app.shutdown().drain().join_report().ensure_clean();
    finish_after_shutdown(result, shutdown)
}

fn run_application(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
) -> anyhow::Result<Report> {
    let worker = Worker {
        // burst 1 → one job, then pace at one per refill window.
        limiter: RateLimit::new("rate_limited_worker.pace", 1, rate_per_sec(), 1),
        report: Report::default(),
        processed: 0,
        pending: 0,
        pacing: false,
        expected: None,
        last_index: None,
    };
    let worker_addr = app
        .register_root::<_, Infallible>(worker, QUEUE_CAPACITY)
        .map_err(|e| anyhow::anyhow!("register worker: {e:?}"))?;

    let waiter = app
        .observe_result::<Report, _, _>(worker_addr)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    // Producer: non-blocking burst. The worker drains the whole
    // command burst before stepping the isolate, so the mailbox fills
    // up to `QUEUE_CAPACITY` and every send past that surfaces
    // `MailboxFull` through the per-send observer the helper installs.
    //
    // the host burst outcome helper: `try_send_outcome` + `HostBurstOutcomes`
    // replace the hand-rolled per-send closure / atomics / observed
    // barrier. The accumulator preserves every truth-typed outcome
    // (admitted, mailbox_full, mailbox_closed, ingress_full,
    // worker_stopped); none of them are collapsed.
    let outcomes = HostBurstOutcomes::new();
    for n in 0..BURST_JOBS {
        let _ = app.try_send_outcome(worker_addr, WorkerMsg::Submit(n), &outcomes);
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
    // the observed-send retry helper: `send_observed_until` is the BurstClosed-style
    // helper. It retries on Full/IngressFull until the deadline and
    // returns typed Closed / WorkerStopped / Timeout. No hidden queue;
    // the control message rides the same bounded data mailbox.
    app.send_observed_until(
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

    Ok(final_report)
}

fn finish_after_shutdown(
    result: anyhow::Result<Report>,
    shutdown: Result<(), tina_runtime::UncleanShutdownError>,
) -> anyhow::Result<Report> {
    match (result, shutdown) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(shutdown_error)) => Err(anyhow::anyhow!(
            "{error:#}; LocalSystem shutdown also failed: {shutdown_error}"
        )),
    }
}
