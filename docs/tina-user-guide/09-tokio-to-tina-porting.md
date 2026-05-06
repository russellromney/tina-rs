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
| `sleep().await` | `sleep(...).reply(...)` |
| socket read/write | `tcp_read` / `tcp_write` effects |
| request task then await answer | `call(..., timeout).reply(...)` |
| retry loop | message state plus timer |
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
            ConnMsg::Begin => tcp_read(self.stream, 4096).reply(ConnMsg::Read),
            ConnMsg::Read(Ok(bytes)) => tcp_write(self.stream, bytes).reply(ConnMsg::Wrote),
            ConnMsg::Read(Err(_)) => stop(),
            ConnMsg::Wrote(_) => stop(),
        }
    }
}
```

Tina is more verbose here.

The trade is explicit state and runtime control.

## Comparison Checklist

When doing an Eiffel port, record:

- what got shorter in Tokio
- what got clearer in Tina
- what got worse in Tina
- what API had to be guessed
- what thing should be helper library
- what overload behavior was visible
- what overload behavior was hidden

Do not defend Tina. Measure Tina.
