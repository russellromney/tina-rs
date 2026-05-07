//! Tina: three isolates — `Producer` ticks out items, `Consumer`
//! drains them, `SignalWatcher` runs `signal_wait("sigint",
//! timeout)` and on receipt sends `Stop` to the producer. The
//! producer respects `Stop` (no new items); the consumer keeps
//! draining until everything in flight is processed.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{CallError, DefaultThreadedMailboxFactory, ThreadedRuntime, signal_wait, sleep};

use crate::{ITEM_INTERVAL_MS, Report, SIGNAL_AFTER_MS, TOTAL_PLANNED_ITEMS};

/// Side channel for the host to read final counts. Same shape as
/// the other examples' app-data slots.
#[derive(Default)]
struct Telemetry {
    produced: AtomicU32,
    processed: AtomicU32,
    signal_received: AtomicBool,
    producer_stopped: AtomicBool,
}

// ---------- Consumer ----------

#[derive(Debug, Clone, Copy)]
enum ConsumerMsg {
    Item(#[allow(dead_code)] u32),
    Done(Result<(), CallError>),
}

struct Consumer {
    telemetry: Arc<Telemetry>,
}

#[tina_runtime::isolate(message = ConsumerMsg)]
impl Consumer {
    fn handle(&mut self, msg: ConsumerMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            ConsumerMsg::Item(_) => sleep(Duration::from_millis(1)).reply(ConsumerMsg::Done),
            ConsumerMsg::Done(Ok(())) => {
                self.telemetry.processed.fetch_add(1, Ordering::Relaxed);
                noop()
            }
            ConsumerMsg::Done(Err(_)) => noop(),
        }
    }
}

// ---------- Producer ----------

#[derive(Debug, Clone, Copy)]
enum ProducerMsg {
    Tick(u32),
    TimerFired(u32, Result<(), CallError>),
    Stop,
}

struct Producer {
    consumer: Address<ConsumerMsg>,
    telemetry: Arc<Telemetry>,
    target: u32,
    stopped: bool,
}

#[tina_runtime::isolate(
    message = ProducerMsg,
    send = Outbound<ConsumerMsg>,
)]
impl Producer {
    fn handle(&mut self, msg: ProducerMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            ProducerMsg::Tick(n) => {
                if self.stopped || n >= self.target {
                    return noop();
                }
                sleep(Duration::from_millis(ITEM_INTERVAL_MS))
                    .reply(move |result| ProducerMsg::TimerFired(n, result))
            }
            ProducerMsg::TimerFired(n, Ok(())) => {
                if self.stopped || n >= self.target {
                    return noop();
                }
                self.telemetry.produced.fetch_add(1, Ordering::Relaxed);
                let next = n + 1;
                batch(vec![
                    send(self.consumer, ConsumerMsg::Item(n)),
                    sleep(Duration::from_millis(ITEM_INTERVAL_MS))
                        .reply(move |result| ProducerMsg::TimerFired(next, result)),
                ])
            }
            ProducerMsg::TimerFired(_, Err(_)) => noop(),
            ProducerMsg::Stop => {
                self.stopped = true;
                self.telemetry
                    .producer_stopped
                    .store(true, Ordering::Release);
                noop()
            }
        }
    }
}

// ---------- SignalWatcher ----------

#[derive(Debug, Clone)]
enum SignalMsg {
    Begin,
    Received(Result<String, CallError>),
}

struct SignalWatcher {
    producer: Address<ProducerMsg>,
    telemetry: Arc<Telemetry>,
}

#[tina_runtime::isolate(
    message = SignalMsg,
    send = Outbound<ProducerMsg>,
)]
impl SignalWatcher {
    fn handle(&mut self, msg: SignalMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            SignalMsg::Begin => {
                signal_wait("sigint", Duration::from_secs(10)).reply(SignalMsg::Received)
            }
            SignalMsg::Received(Ok(_)) => {
                self.telemetry
                    .signal_received
                    .store(true, Ordering::Release);
                send(self.producer, ProducerMsg::Stop)
            }
            SignalMsg::Received(Err(_)) => stop(),
        }
    }
}

// ---------- Run ----------

pub fn run() -> anyhow::Result<Report> {
    let runtime = ThreadedRuntime::new(SingleShard, DefaultThreadedMailboxFactory);
    let telemetry = Arc::new(Telemetry::default());

    let consumer = runtime
        .register_with_capacity::<_, Infallible>(
            Consumer {
                telemetry: Arc::clone(&telemetry),
            },
            (TOTAL_PLANNED_ITEMS as usize) + 4,
        )
        .map_err(|e| anyhow::anyhow!("register consumer: {e:?}"))?;

    let producer = runtime
        .register_with_capacity::<_, _>(
            Producer {
                consumer,
                telemetry: Arc::clone(&telemetry),
                target: TOTAL_PLANNED_ITEMS,
                stopped: false,
            },
            16,
        )
        .map_err(|e| anyhow::anyhow!("register producer: {e:?}"))?;

    let watcher = runtime
        .register_with_capacity::<_, _>(
            SignalWatcher {
                producer,
                telemetry: Arc::clone(&telemetry),
            },
            8,
        )
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

    // Wait for: signal observed AND producer stopped AND consumer
    // has caught up. App-data side channel — same pattern as
    // `eiffel_persistent_counter::Observation`.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stopped = telemetry.producer_stopped.load(Ordering::Acquire);
        let produced = telemetry.produced.load(Ordering::Acquire);
        let processed = telemetry.processed.load(Ordering::Acquire);
        let signal = telemetry.signal_received.load(Ordering::Acquire);
        if stopped && signal && processed >= produced && produced > 0 {
            // One last yield in case a `Done` is in flight.
            thread::sleep(Duration::from_millis(5));
            if telemetry.processed.load(Ordering::Acquire) == produced {
                break;
            }
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "graceful drain timed out: signal={signal} stopped={stopped} produced={produced} processed={processed}",
            );
        }
        thread::yield_now();
    }

    let _ = runtime.shutdown();

    Ok(Report {
        items_produced: telemetry.produced.load(Ordering::Relaxed),
        items_processed: telemetry.processed.load(Ordering::Relaxed),
        signal_received: telemetry.signal_received.load(Ordering::Acquire),
        items_remaining_in_queue_at_exit: 0,
        exit_clean: true,
    })
}
