use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Mutex;
use std::time::Duration;

use tina::prelude::*;
use tina::{Mailbox, TrySendError};
use tina_runtime::{BetelgeuseBackedRuntimeConfig, MailboxFactory};
use tina_tokio_bridge::{BridgeHost, BridgeRequest};

#[derive(Debug, Clone, Copy)]
struct BarnShard;

impl Shard for BarnShard {
    fn id(&self) -> ShardId {
        ShardId::new(1)
    }
}

struct BarnMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> BarnMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for BarnMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(TrySendError::Closed(message));
        }
        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }
        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue lock").pop_front()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct BarnMailboxFactory;

impl MailboxFactory for BarnMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(BarnMailbox::new(capacity))
    }
}

#[derive(Debug, Clone)]
struct LlamaQuestion {
    snack: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LlamaAnswer {
    line: String,
}

#[derive(Debug)]
struct Tina {
    snacks_eaten: usize,
}

#[tina::isolate(
    message = BridgeRequest<LlamaQuestion, LlamaAnswer>,
    shard = BarnShard
)]
impl Tina {
    fn handle(
        &mut self,
        msg: BridgeRequest<LlamaQuestion, LlamaAnswer>,
        _ctx: &mut Context<'_, BarnShard>,
    ) -> Effect<Self> {
        let (question, responder) = msg.into_parts();
        self.snacks_eaten += 1;
        let _ = responder.respond(LlamaAnswer {
            line: format!("ate {} ({})", question.snack, self.snacks_eaten),
        });
        noop()
    }
}

fn main() {
    let host = BridgeHost::new(
        BarnShard,
        BarnMailboxFactory,
        BetelgeuseBackedRuntimeConfig {
            command_capacity: 16,
            idle_wait: Duration::from_millis(1),
            ..Default::default()
        },
    );
    let bridge = host
        .register_bridge::<Tina, LlamaQuestion, LlamaAnswer, Infallible>(
            Tina { snacks_eaten: 0 },
            16,
            Duration::from_secs(1),
        )
        .expect("register Tina");

    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio runtime");
    let answer = tokio
        .block_on(bridge.call(LlamaQuestion { snack: "ham" }))
        .expect("bridge response");

    assert_eq!(answer.line, "ate ham (1)");
    println!("{}", answer.line);

    assert_eq!(bridge.metrics().responses, 1);
    drop(bridge);
    host.shutdown().expect("shutdown");
}
