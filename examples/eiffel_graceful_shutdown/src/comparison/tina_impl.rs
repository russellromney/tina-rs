use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_runtime::{
    CallError, DefaultThreadedMailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig, signal_wait,
    sleep,
};

use super::{ITEM_INTERVAL_MS, SIGNAL_AFTER_MS, SideReport, TOTAL_PLANNED_ITEMS};

#[derive(Default)]
struct Telemetry {
    produced: AtomicU32,
    processed: AtomicU32,
    signal_received: AtomicBool,
    producer_stopped: AtomicBool,
}

#[derive(Debug, Clone, Copy)]
enum ConsumerMsg {
    Item(u32),
    Done(Result<(), CallError>),
}

struct Consumer {
    telemetry: Arc<Telemetry>,
}

#[tina_runtime::isolate(message = ConsumerMsg)]
impl Consumer {
    fn handle(&mut self, msg: ConsumerMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            ConsumerMsg::Item(_n) => sleep(Duration::from_millis(1)).reply(ConsumerMsg::Done),
            ConsumerMsg::Done(result) => {
                if result.is_ok() {
                    self.telemetry.processed.fetch_add(1, Ordering::Relaxed);
                }
                noop()
            }
        }
    }
}

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
            ProducerMsg::TimerFired(n, result) => {
                if self.stopped || n >= self.target || result.is_err() {
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
            SignalMsg::Received(Ok(_name)) => {
                self.telemetry
                    .signal_received
                    .store(true, Ordering::Release);
                send(self.producer, ProducerMsg::Stop)
            }
            SignalMsg::Received(Err(_)) => stop(),
        }
    }
}

pub(crate) fn run() -> SideReport {
    let runtime = ThreadedRuntime::with_config(
        SingleShard,
        DefaultThreadedMailboxFactory,
        ThreadedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );

    let telemetry = Arc::new(Telemetry::default());

    let consumer = runtime
        .register_with_capacity::<Consumer, Infallible>(
            Consumer {
                telemetry: Arc::clone(&telemetry),
            },
            (TOTAL_PLANNED_ITEMS as usize) + 4,
        )
        .expect("register consumer");

    let producer = runtime
        .register_with_capacity::<Producer, _>(
            Producer {
                consumer,
                telemetry: Arc::clone(&telemetry),
                target: TOTAL_PLANNED_ITEMS,
                stopped: false,
            },
            16,
        )
        .expect("register producer");

    let watcher = runtime
        .register_with_capacity::<SignalWatcher, _>(
            SignalWatcher {
                producer,
                telemetry: Arc::clone(&telemetry),
            },
            8,
        )
        .expect("register signal watcher");

    runtime
        .try_send(producer, ProducerMsg::Tick(0))
        .expect("kick producer");
    runtime
        .try_send(watcher, SignalMsg::Begin)
        .expect("kick watcher");

    // Operator: simulate a real Ctrl-C from outside the runtime.
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(SIGNAL_AFTER_MS));
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("raise SIGINT");
    });

    // Wait for: signal observed AND producer marked stopped AND consumer has
    // caught up with producer.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stopped = telemetry.producer_stopped.load(Ordering::Acquire);
        let produced = telemetry.produced.load(Ordering::Acquire);
        let processed = telemetry.processed.load(Ordering::Acquire);
        let signal = telemetry.signal_received.load(Ordering::Acquire);
        if stopped && signal && processed >= produced && produced > 0 {
            // Allow a couple of cycles for any in-flight Done message.
            thread::sleep(Duration::from_millis(5));
            let processed_again = telemetry.processed.load(Ordering::Acquire);
            if processed_again == produced {
                break;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "graceful drain timed out: signal={signal} stopped={stopped} produced={produced} processed={processed}"
            );
        }
        thread::yield_now();
    }

    let _ = runtime.shutdown().expect("runtime shutdown");

    SideReport {
        items_produced: telemetry.produced.load(Ordering::Relaxed),
        items_processed: telemetry.processed.load(Ordering::Relaxed),
        signal_received: telemetry.signal_received.load(Ordering::Acquire),
        items_remaining_in_queue_at_exit: 0,
        exit_clean: true,
    }
}
