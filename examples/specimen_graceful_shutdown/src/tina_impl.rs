//! Tina: three isolates — `Producer` ticks out items, `Consumer`
//! drains them, `SignalWatcher` runs `signal_wait("sigint",
//! timeout)` and on receipt sends `Stop` to the producer. The
//! producer respects `Stop` (no new items) and tells the consumer
//! the final produced count; the consumer keeps draining until every
//! in-flight item is processed, then `stop_with(Report)`. The host
//! claims that report with `observe_result` before start.

use std::convert::Infallible;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{
    DefaultThreadedMailboxFactory, SignalWaitReply, SleepReply, ThreadedRuntime, signal_wait, sleep,
};

use crate::{ITEM_INTERVAL_MS, Report, SIGNAL_AFTER_MS, TOTAL_PLANNED_ITEMS};

// ---------- Consumer ----------

#[derive(Debug, Clone, Copy)]
enum ConsumerMsg {
    Item(#[allow(dead_code)] u32),
    Done(SleepReply),
    /// Producer finished: either signal-stop or natural end. Carries the
    /// final produced count so the consumer can drain exactly that far.
    ProducerDone { produced: u32, signal_received: bool },
}

struct Consumer {
    processed: u32,
    expected: Option<u32>,
    signal_received: bool,
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
            } => {
                self.expected = Some(produced);
                self.signal_received = signal_received;
                self.maybe_finish()
            }
        }
    }
}

impl Consumer {
    fn maybe_finish(&self) -> Effect<Self> {
        match self.expected {
            Some(produced) if self.processed >= produced => stop_with(Report {
                items_produced: produced,
                items_processed: self.processed,
                signal_received: self.signal_received,
                items_remaining_in_queue_at_exit: 0,
                exit_clean: true,
            }),
            _ => noop(),
        }
    }
}

// ---------- Producer ----------

#[derive(Debug, Clone, Copy)]
enum ProducerMsg {
    Tick(u32),
    TimerFired(u32, SleepReply),
    Stop,
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
            ProducerMsg::TimerFired(_, Err(_)) => noop(),
            ProducerMsg::Stop => {
                self.stopped = true;
                self.signal_stop = true;
                send(
                    self.consumer,
                    ConsumerMsg::ProducerDone {
                        produced: self.produced,
                        signal_received: true,
                    },
                )
            }
        }
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
            SignalMsg::Received(Ok(_)) => send(self.producer, ProducerMsg::Stop),
            SignalMsg::Received(Err(_)) => stop(),
        }
    }
}

// ---------- Run ----------

pub fn run() -> anyhow::Result<Report> {
    let runtime = Arc::new(ThreadedRuntime::try_new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    )?);
    let shutdown = runtime.shutdown_handle();

    let consumer = runtime
        .register_with_capacity::<_, Infallible>(
            Consumer {
                processed: 0,
                expected: None,
                signal_received: false,
            },
            (TOTAL_PLANNED_ITEMS as usize) + 4,
        )
        .map_err(|e| anyhow::anyhow!("register consumer: {e:?}"))?;

    // Claim the terminal drain report before any message can stop the consumer.
    let waiter = runtime
        .observe_result::<Report, _, _>(consumer)
        .map_err(|e| anyhow::anyhow!("observe_result: {e:?}"))?;

    let producer = runtime
        .register_with_capacity::<_, _>(
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

    let watcher = runtime
        .register_with_capacity::<_, _>(SignalWatcher { producer }, 8)
        .map_err(|e| anyhow::anyhow!("register watcher: {e:?}"))?;

    runtime
        .try_send(producer, ProducerMsg::Tick(0))
        .map_err(|e| anyhow::anyhow!("kick producer: {e:?}"))?;
    runtime
        .try_send(watcher, SignalMsg::Begin)
        .map_err(|e| anyhow::anyhow!("kick watcher: {e:?}"))?;

    // Operator: simulate Ctrl-C from outside the runtime.
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(SIGNAL_AFTER_MS));
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("raise SIGINT");
    });

    let report = waiter
        .wait(Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("consumer did not finish drain: {e:?}"))?;

    let terminal = shutdown.request_and_wait_report(Duration::from_secs(5))?;
    drop(runtime);
    terminal.ensure_clean()?;

    Ok(report)
}
