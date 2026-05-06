# Boundedness And Overload

Tina should make overload boring and visible.

Important words:

- accepted
- full
- closed
- timeout

If a system is under pressure, it should shed load explicitly instead of
quietly growing hidden queues.

## Mailbox Capacity

Every isolate mailbox has capacity.

When spawning children, pass a capacity:

```rust
spawn(ChildDefinition::new(child, 32))
```

Small capacity is good for testing.

If capacity is `1`, the second queued message should hit pressure.

## Observed Send

Plain `send` is simple but does not tell sender what happened.

Use observed send when overload matters:

```rust
use tina_runtime::{send_observed, SendOutcome};

#[derive(Debug, Clone)]
enum ProducerMsg {
    Burst(usize),
    Sent(SendOutcome),
}

#[tina_runtime::isolate(
    message = ProducerMsg,
    send = Outbound<ConsumerMsg>,
    shard = AppShard
)]
impl Producer {
    fn handle(&mut self, msg: ProducerMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ProducerMsg::Burst(n) => batch(
                (0..n)
                    .map(|i| {
                        send_observed(self.consumer, ConsumerMsg::Item(i))
                            .reply(ProducerMsg::Sent)
                    })
                    .collect(),
            ),
            ProducerMsg::Sent(outcome) => {
                if outcome.is_full() {
                    self.rejected += 1;
                }
                noop()
            }
        }
    }
}
```

This is the heart of the Tokio comparison.

Tokio often accepts into a channel or spawned task until pressure appears
somewhere else.

Tina should be able to say:

```text
accepted=12000 full=38000 timeouts=0 exit=clean
```

## Timeout Is Load Control

Request/reply uses timeout:

```rust
call(worker, WorkerMsg::Run(job), Duration::from_millis(20))
    .reply(ClientMsg::Done)
```

Handle timeout as normal behavior.

Do not panic on timeout in service code.

## Comparison Rule

For every Eiffel comparison, collect:

- accepted work
- rejected full
- closed
- timeouts
- peak pending if easy
- exit status
- crude RSS if the platform gives it

First pass can run without hard memory caps. Later pass uses Linux/Fly/Docker
limits.

## What Counts As Failure

Good failure:

```text
server says full
client gets response
process stays alive
metrics make sense
```

Bad failure:

```text
process OOMs
latency goes strange
no overload signal exists
shutdown hangs
metrics lie
```

When Tina fails badly, write it down. That is the work.
