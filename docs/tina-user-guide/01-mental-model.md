# Mental Model

Tokio says:

```rust
async fn work() {
    read().await;
    write().await;
}
```

Tina says:

```text
message in
update owned state
return effect
runtime later sends another message
```

No `.await` in isolate handlers.

## The Pieces

An isolate is one little state machine.

It has:

- owned state
- a message type
- optional reply type
- optional outbound message types
- optional child spawn type
- optional runtime call type

The runtime owns scheduling, time, I/O, calls, and supervision.

The isolate only says what should happen next.

## Why This Exists

Tokio makes it easy to accept work faster than the process can finish it.

Tina wants backpressure to be visible:

```text
send accepted
send full
send closed
call timed out
```

This is the main contract. Hidden unbounded work is bad. Explicit load shedding
is good.

## Handler Rule

A handler should be boring:

```rust
fn handle(&mut self, msg: Msg, ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
    match msg {
        Msg::A => noop(),
        Msg::B => send(self.other, OtherMsg::B),
        Msg::C => reply(42),
    }
}
```

Do not block. Do not sleep. Do not read sockets directly. Do not spawn random
threads from handlers.

Ask Tina runtime instead.

## Good Tina Shape

Good:

- one owner per state blob
- one message enum per protocol
- explicit timeout for request/reply
- explicit mailbox capacity
- explicit runtime effect for I/O
- explicit overload handling

Bad:

- `Arc<Mutex<_>>` as normal app state
- unbounded channel inside isolate
- blocking operation in handler
- handler doing real I/O itself
- ignoring `Full`
- assuming send means delivery

Some `Arc<Mutex<_>>` exists in examples for test harness observation. That is
not app shape. It is harness shape.
