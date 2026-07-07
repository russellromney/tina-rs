# Tokio To Tina Porting

Port small pieces first.

Do not start with a giant async service. Start with one protocol loop or one
queueing component.

## Mapping

| Tokio thing | Tina thing |
| --- | --- |
| `tokio::spawn` | child isolate |
| async task state | isolate struct fields |
| `.await` continuation | next message variant |
| `mpsc::channel` | bounded mailbox |
| unbounded channel | usually a bug or explicit adapter |
| `select!` | state machine plus runtime call replies |
| `sleep().await` | `sleep(...).then(...)` |
| `tokio::time::interval` | `RecurringTick` state plus `sleep(delay).then(...)` |
| socket read/write | `tcp_read` / `tcp_write` effects |
| request task then await answer | `call(..., timeout).then(...)` |
| retry loop | message state plus `Backoff` and timer |
| task panic | supervised child stop/restart |

## Porting Steps

1. Find the owned state.
2. Make it an isolate struct.
3. Find every await point.
4. Turn each await point into a message variant.
5. Replace I/O with runtime calls.
6. Replace channels with addresses and mailbox capacity.
7. Decide where overload is accepted, rejected, or timed out.
8. Add one sim test.
9. Add one real I/O example if it is a network thing.

## Example

Tokio:

```rust
async fn handle(mut stream: TcpStream) -> io::Result<()> {
    let mut buf = vec![0; 4096];
    let n = stream.read(&mut buf).await?;
    stream.write_all(&buf[..n]).await?;
    Ok(())
}
```

Tina:

```rust
enum ConnMsg {
    Begin,
    Read(Result<Vec<u8>, CallError>),
    Wrote(Result<usize, CallError>),
}

impl Conn {
    fn handle(&mut self, msg: ConnMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ConnMsg::Begin => tcp_read(self.stream, 4096).then(ConnMsg::Read),
            ConnMsg::Read(Ok(bytes)) => tcp_write(self.stream, bytes).then(ConnMsg::Wrote),
            ConnMsg::Read(Err(_)) => stop(),
            ConnMsg::Wrote(_) => stop(),
        }
    }
}
```

Tina is more verbose here.

The trade is explicit state and runtime control.

## Timers

Tokio timer loops often hide policy in control flow:

```rust
loop {
    tokio::select! {
        _ = interval.tick() => flush(),
        result = work() => maybe_retry(result),
    }
}
```

In Tina, name that policy as state:

```rust
let decision = self.interval.next_delay(ctx.now());
sleep(decision.delay()).then(move |reply| Msg::Tick(decision.tick_number(), reply))
```

For retries:

```rust
match self.backoff.next_delay_until(ctx.now(), self.deadline) {
    TimerDecision::Sleep(delay) => {
        sleep(delay.delay()).then(move |reply| Msg::Retry(delay.attempt(), reply))
    }
    TimerDecision::DeadlineElapsed | TimerDecision::Exhausted => reply(Failed),
}
```

The important split is unchanged: helper decides delay, user returns the
visible sleep effect, continuation handles the result and records any
user-visible outcome. Missed ticks and exhausted attempts stay in ordinary user
code where replay can see them.

## Comparison Checklist

When porting Tokio-shaped code, record:

- what got shorter in Tokio
- what got clearer in Tina
- what got worse in Tina
- what API had to be guessed
- what thing should be helper library
- what overload behavior was visible
- what overload behavior was hidden

Do not defend Tina. Measure Tina.
