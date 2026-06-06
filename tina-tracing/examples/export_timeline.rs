use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use tina::prelude::*;
use tina::{CallContext, Mailbox, ShardId, TrySendError};
use tina_runtime::{CallOutcome, MailboxFactory, Runtime, RuntimeCall, RuntimeEventKind, call};
use tina_tracing::{TraceTimeline, write_chrome_trace_json};

#[derive(Debug, Clone, Copy)]
struct DemoShard;

impl Shard for DemoShard {
    fn id(&self) -> ShardId {
        ShardId::new(0)
    }
}

struct DemoMailbox<T> {
    capacity: usize,
    queue: Rc<RefCell<VecDeque<T>>>,
    closed: Rc<Cell<bool>>,
}

impl<T> DemoMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Rc::new(RefCell::new(VecDeque::new())),
            closed: Rc::new(Cell::new(false)),
        }
    }
}

impl<T> Mailbox<T> for DemoMailbox<T> {
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

    fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }

    fn close(&self) {
        self.closed.set(true);
    }
}

#[derive(Debug, Clone, Copy)]
struct DemoMailboxFactory;

impl MailboxFactory for DemoMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(DemoMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy)]
enum WorkerMsg {
    Ping,
}

#[derive(Debug, Clone, Copy)]
struct WorkerReply;

#[derive(Debug)]
struct Worker;

impl Isolate for Worker {
    tina::isolate_types! {
        message: WorkerMsg,
        reply: WorkerReply,
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: Infallible,
        shard: DemoShard,
    }

    fn handle(
        &mut self,
        _msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        noop()
    }

    fn handle_call(&mut self, _msg: Self::Message, call: CallContext<'_, Self>) -> Effect<Self> {
        call.reply(WorkerReply)
    }
}

#[derive(Debug)]
enum DriverMsg {
    Start(Address<WorkerMsg, WorkerReply>),
    Returned(CallOutcome<WorkerReply>),
}

#[derive(Debug)]
struct Driver {
    outcomes: Rc<RefCell<Vec<CallOutcome<WorkerReply>>>>,
}

impl Isolate for Driver {
    tina::isolate_types! {
        message: DriverMsg,
        reply: (),
        send: Outbound<Infallible>,
        spawn: Infallible,
        call: RuntimeCall<DriverMsg>,
        shard: DemoShard,
    }

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            DriverMsg::Start(worker) => {
                call(worker, WorkerMsg::Ping, Duration::from_millis(50)).then(DriverMsg::Returned)
            }
            DriverMsg::Returned(outcome) => {
                self.outcomes.borrow_mut().push(outcome);
                noop()
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::new(DemoShard, DemoMailboxFactory);
    let worker = runtime.register(Worker, DemoMailbox::new(8));
    let outcomes = Rc::new(RefCell::new(Vec::new()));
    let driver = runtime.register(
        Driver {
            outcomes: Rc::clone(&outcomes),
        },
        DemoMailbox::new(8),
    );

    runtime
        .try_send(driver, DriverMsg::Start(worker))
        .expect("demo driver mailbox should admit the start message");
    while outcomes.borrow().is_empty() {
        runtime.step();
    }

    let has_call = runtime
        .trace()
        .iter()
        .any(|event| matches!(event.kind(), RuntimeEventKind::CallCompleted { .. }));
    assert!(has_call, "demo trace should include one completed call");

    let timeline = TraceTimeline::from_events(runtime.trace())
        .with_name("export_timeline demo")
        .finish();
    let path = PathBuf::from("target/tina-traces/export_timeline.trace.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_chrome_trace_json(&timeline, &path)?;
    println!("{}", path.display());
    Ok(())
}
