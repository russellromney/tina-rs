use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    CallError, MailboxFactory, ThreadedRuntime, ThreadedRuntimeConfig, signal_wait, sleep,
};

use super::{ITEM_INTERVAL_MS, SIGNAL_AFTER_MS, SideReport, TOTAL_PLANNED_ITEMS};

#[derive(Debug, Default)]
struct ShutdownShard;

impl Shard for ShutdownShard {
    fn id(&self) -> ShardId {
        ShardId::new(64)
    }
}

struct ShutdownMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> ShutdownMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for ShutdownMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if self.closed.get() {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.borrow_mut();
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.borrow_mut().pop_front()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct ShutdownMailboxFactory;

impl MailboxFactory for ShutdownMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(ShutdownMailbox::new(capacity))
    }
}

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

#[tina_runtime::isolate(message = ConsumerMsg, shard = ShutdownShard)]
impl Consumer {
    fn handle(&mut self, msg: ConsumerMsg, _ctx: &mut Context<'_, ShutdownShard>) -> Effect<Self> {
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
    shard = ShutdownShard
)]
impl Producer {
    fn handle(&mut self, msg: ProducerMsg, _ctx: &mut Context<'_, ShutdownShard>) -> Effect<Self> {
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
    shard = ShutdownShard
)]
impl SignalWatcher {
    fn handle(&mut self, msg: SignalMsg, _ctx: &mut Context<'_, ShutdownShard>) -> Effect<Self> {
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
        ShutdownShard,
        ShutdownMailboxFactory,
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
