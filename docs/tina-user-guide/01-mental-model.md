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

## What Tina Actually Enforces

The "owned state" claim is about what the *isolate model* enforces.
Not about what the user can build in safe Rust.

Tina's normal typed paths do not require shared mutable state. Rust
still lets users opt out. An `Arc<Mutex<T>>` built by the user and
passed into an isolate field compiles and runs. The runtime cannot
detect that.

What the model enforces:

- every cross-isolate exchange is a typed message;
- the type system blocks the obvious leaks: non-`Send` state across
  thread-sharded runtimes, references escaping into `'static`
  continuation closures, non-`Send` payloads in messages,
  non-`Send` child isolate state;
- a user who reaches for `Arc<Mutex<_>>` between isolates has
  explicitly opted out.

The adversarial probe in
[`examples/eiffel_owned_state_leak`](../../examples/eiffel_owned_state_leak)
documents four leak attempts the type system blocks and one
user-built shared-state pattern that compiles. The four blocks
live as `compile_fail` doctests. The smoke test asserts the
user-built `Arc<Mutex<_>>` did get incremented — proof the type
system did not block it. Both halves are positive evidence: the
type system enforces what it promises, and no more.
