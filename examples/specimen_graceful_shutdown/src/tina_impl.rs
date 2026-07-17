//! Tina: three isolates — `Producer` ticks out items, `Consumer`
//! drains them, `SignalWatcher` runs `signal_wait("sigint",
//! timeout)` and on receipt sends `Stop` to the producer. The
//! producer respects `Stop` (no new items) and tells the consumer
//! the final produced count; the consumer keeps draining until every
//! in-flight item is processed, then `stop_with(Report)`. The host
//! claims that report with `observe_result` before start.

use std::convert::Infallible;
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, LocalSystem, SignalWaitReply, SleepReply, signal_wait, sleep,
};

use crate::{ITEM_INTERVAL_MS, Report, SIGNAL_AFTER_MS, TOTAL_PLANNED_ITEMS};

// ---------- Consumer ----------

#[derive(Debug, Clone, Copy)]
enum ConsumerMsg {
    Item(#[allow(dead_code)] u32),
    Done(SleepReply),
    /// Producer finished: either signal-stop or natural end. Carries the
    /// final produced count so the consumer can drain exactly that far.
    ProducerDone {
        produced: u32,
        signal_received: bool,
        exit_clean: bool,
    },
}

struct Consumer {
    processed: u32,
    expected: Option<u32>,
    signal_received: bool,
    exit_clean: bool,
}

#[tina_runtime::isolate(message = ConsumerMsg)]
impl Consumer {
    fn handle(
        &mut self,
        msg: ConsumerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ConsumerMsg::Item(_) => sleep(Duration::from_millis(1)).then(ConsumerMsg::Done),
            ConsumerMsg::Done(Ok(())) => {
                self.processed += 1;
                self.maybe_finish()
            }
            ConsumerMsg::Done(Err(_)) => {
                // Work timer cancelled (shutdown). Report what we have rather
                // than pretend a clean drain.
                stop_with(Report {
                    items_produced: self.expected.unwrap_or(self.processed),
                    items_processed: self.processed,
                    signal_received: self.signal_received,
                    items_remaining_in_queue_at_exit: self
                        .expected
                        .map(|e| e.saturating_sub(self.processed))
                        .unwrap_or(0),
                    exit_clean: false,
                })
            }
            ConsumerMsg::ProducerDone {
                produced,
                signal_received,
                exit_clean,
            } => {
                self.expected = Some(produced);
                self.signal_received = signal_received;
                self.exit_clean = exit_clean;
                self.maybe_finish()
            }
        }
    }
}

impl Consumer {
    fn maybe_finish(&self) -> Effect<Self> {
        match self.finished_report() {
            Some(report) => stop_with(report),
            None => noop(),
        }
    }

    fn finished_report(&self) -> Option<Report> {
        let produced = self.expected?;
        (self.processed >= produced).then_some(Report {
            items_produced: produced,
            items_processed: self.processed,
            signal_received: self.signal_received,
            items_remaining_in_queue_at_exit: 0,
            exit_clean: self.exit_clean,
        })
    }
}

// ---------- Producer ----------

#[derive(Debug, Clone, Copy)]
enum ProducerMsg {
    Tick(u32),
    TimerFired(u32, SleepReply),
    Stop,
    SignalFailed,
}

struct Producer {
    consumer: Address<ConsumerMsg>,
    target: u32,
    produced: u32,
    stopped: bool,
    signal_stop: bool,
}

#[tina_runtime::isolate(
    message = ProducerMsg,
    send = Outbound<ConsumerMsg>,
)]
impl Producer {
    fn handle(
        &mut self,
        msg: ProducerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ProducerMsg::Tick(n) => {
                if self.stopped || n >= self.target {
                    return noop();
                }
                sleep(Duration::from_millis(ITEM_INTERVAL_MS))
                    .then(move |result| ProducerMsg::TimerFired(n, result))
            }
            ProducerMsg::TimerFired(n, Ok(())) => {
                if self.stopped || n >= self.target {
                    return noop();
                }
                self.produced += 1;
                let next = n + 1;
                if next >= self.target {
                    // Natural end after the final item: hand the count to the
                    // consumer so it can drain without a signal.
                    batch(vec![
                        send(self.consumer, ConsumerMsg::Item(n)),
                        send(
                            self.consumer,
                            ConsumerMsg::ProducerDone {
                                produced: self.produced,
                                signal_received: self.signal_stop,
                                exit_clean: true,
                            },
                        ),
                    ])
                } else {
                    batch(vec![
                        send(self.consumer, ConsumerMsg::Item(n)),
                        sleep(Duration::from_millis(ITEM_INTERVAL_MS))
                            .then(move |result| ProducerMsg::TimerFired(next, result)),
                    ])
                }
            }
            ProducerMsg::TimerFired(_, Err(_)) => {
                self.stopped = true;
                send(self.consumer, dependency_failure(self.produced))
            }
            ProducerMsg::Stop => {
                self.stopped = true;
                self.signal_stop = true;
                send(
                    self.consumer,
                    ConsumerMsg::ProducerDone {
                        produced: self.produced,
                        signal_received: true,
                        exit_clean: true,
                    },
                )
            }
            ProducerMsg::SignalFailed => {
                self.stopped = true;
                send(self.consumer, dependency_failure(self.produced))
            }
        }
    }
}

fn dependency_failure(produced: u32) -> ConsumerMsg {
    ConsumerMsg::ProducerDone {
        produced,
        signal_received: false,
        exit_clean: false,
    }
}

// ---------- SignalWatcher ----------

#[derive(Debug, Clone)]
enum SignalMsg {
    Begin,
    Received(SignalWaitReply),
}

struct SignalWatcher {
    producer: Address<ProducerMsg>,
}

#[tina_runtime::isolate(
    message = SignalMsg,
    send = Outbound<ProducerMsg>,
)]
impl SignalWatcher {
    fn handle(
        &mut self,
        msg: SignalMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SignalMsg::Begin => {
                signal_wait("sigint", Duration::from_secs(10)).then(SignalMsg::Received)
            }
            SignalMsg::Received(result) => send(self.producer, signal_completion(result)),
        }
    }
}

fn signal_completion(result: SignalWaitReply) -> ProducerMsg {
    match result {
        Ok(_) => ProducerMsg::Stop,
        Err(_) => ProducerMsg::SignalFailed,
    }
}

// ---------- Run ----------

pub fn run() -> anyhow::Result<Report> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    app.run_to_shutdown_reported(Duration::from_secs(5), |app| {
        let consumer = app
            .register_root::<_, Infallible>(
            Consumer {
                processed: 0,
                expected: None,
                signal_received: false,
                exit_clean: true,
            },
            (TOTAL_PLANNED_ITEMS as usize) + 4,
        )
        .map_err(|e| anyhow::anyhow!("register consumer: {e:?}"))?;

        // Claim the terminal drain report before any message can stop the consumer.
        let waiter = app
            .observe_result::<Report, _, _>(consumer)
            .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

        let producer = app
            .register_root::<_, _>(
            Producer {
                consumer,
                target: TOTAL_PLANNED_ITEMS,
                produced: 0,
                stopped: false,
                signal_stop: false,
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register producer: {e:?}"))?;

        let watcher = app
            .register_root::<_, _>(SignalWatcher { producer }, 8)
            .map_err(|e| anyhow::anyhow!("register watcher: {e:?}"))?;

        app
            .try_send(producer, ProducerMsg::Tick(0))
            .map_err(|e| anyhow::anyhow!("kick producer: {e:?}"))?;
        app
            .try_send(watcher, SignalMsg::Begin)
            .map_err(|e| anyhow::anyhow!("kick watcher: {e:?}"))?;

        // Operator: simulate Ctrl-C from outside the runtime.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(SIGNAL_AFTER_MS));
            signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("raise SIGINT");
        });

        waiter
            .wait(Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("consumer did not finish drain: {e:?}"))
    })
    .map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_dependency_failure_is_not_a_clean_stop() {
        assert!(matches!(
            signal_completion(Err(tina_runtime::CallError::Unsupported)),
            ProducerMsg::SignalFailed
        ));
        assert!(matches!(
            signal_completion(Ok("sigint".to_owned())),
            ProducerMsg::Stop
        ));

        assert!(matches!(
            dependency_failure(3),
            ConsumerMsg::ProducerDone {
                produced: 3,
                signal_received: false,
                exit_clean: false,
            }
        ));
    }

    #[test]
    fn dependency_failure_still_drains_before_reporting_unclean() {
        let mut consumer = Consumer {
            processed: 2,
            expected: Some(3),
            signal_received: false,
            exit_clean: false,
        };
        assert!(consumer.finished_report().is_none());
        consumer.processed = 3;
        let report = consumer.finished_report().expect("all admitted work drained");
        assert_eq!(report.items_produced, 3);
        assert_eq!(report.items_processed, 3);
        assert_eq!(report.items_remaining_in_queue_at_exit, 0);
        assert!(!report.signal_received);
        assert!(!report.exit_clean, "dependency failure must never look clean");
    }
}
