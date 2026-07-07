# TCP And Unix Loops

`tcp_write` / `unix_write` may write fewer bytes than requested.
`tcp_read` / `unix_read` return one chunk at a time, with an empty
chunk meaning EOF. These are the truthful one-shot primitives.

For normal service code, use the small loop helpers:

- `TcpWriteAll` — keep writing until the buffer is sent or an error
  happens.
- `TcpReadToEof` — keep reading until EOF, while enforcing a total cap.
- `UnixWriteAll` — same shape for Unix-domain sockets.
- `UnixReadToEof` — same shape for Unix-domain sockets.

The helper does not hide runtime work. Each step still returns one
effect and later one continuation message.

## Same-Stream Batch Caveat

`tina::batch(...)` (and `tina::batch(...)`) execute effects in
source order, but they do **not** serialize calls that target the same
I/O resource. Two writes against the same stream in one batch still
compete for the same write lane. The second one returns
`CallError::ResourceBusy`.

```rust,ignore
// WRONG: both calls target the same stream write lane.
batch(vec![
    tcp_write(stream, request_a).then(MyMsg::WroteA),
    tcp_write(stream, request_b).then(MyMsg::WroteB),
])
```

Chain same-resource work through continuation messages or use
`TcpWriteAll`.

## Write All

```rust,ignore
match msg {
    Msg::StartWrite(bytes) => {
        let helper = TcpWriteAll::new(stream, bytes);
        let effect = helper.next_effect(Msg::Wrote).unwrap_or_else(noop);
        self.write = Some(helper);
        effect
    }
    Msg::Wrote(reply) => {
        let helper = self.write.as_mut().expect("write armed");
        match helper.advance(reply, Msg::Wrote) {
            LoopStep::Pending(effect) => effect,
            LoopStep::Done(report) => {
                self.write = None;
                self.after_write(report)
            }
            LoopStep::Failed(error) => self.fail(error),
        }
    }
}
```

## Read To EOF

```rust,ignore
match msg {
    Msg::StartRead => {
        let helper = TcpReadToEof::new(stream, 64 * 1024, 4096);
        let effect = helper.next_effect(Msg::Read).unwrap_or_else(noop);
        self.read = Some(helper);
        effect
    }
    Msg::Read(reply) => {
        let helper = self.read.as_mut().expect("read armed");
        match helper.advance(reply, Msg::Read) {
            LoopStep::Pending(effect) => effect,
            LoopStep::Done(bytes) => {
                self.read = None;
                self.got_body(bytes)
            }
            LoopStep::Failed(error) => self.fail(error),
        }
    }
}
```

`UnixWriteAll` and `UnixReadToEof` use the same shape with Unix socket
IDs.

## File Copy

For file rails, `FileCopyBounded` now owns the read/write alternation:

```rust,ignore
match msg {
    Msg::StartCopy => {
        let helper = FileCopyBounded::new(src, dst, 0, 0, 8192, 10 * 1024 * 1024);
        let effect = helper
            .next_effect(Msg::ReadCopied, Msg::WriteCopied)
            .unwrap_or_else(noop);
        self.copy = Some(helper);
        effect
    }
    Msg::ReadCopied(reply) => {
        let helper = self.copy.as_mut().expect("copy armed");
        match helper.advance(
            FileCopyProgress::Read(reply),
            Msg::ReadCopied,
            Msg::WriteCopied,
        ) {
            FileCopyStep::Pending(effect) => effect,
            FileCopyStep::Done(report) => self.finish_copy(report),
        }
    }
    Msg::WriteCopied(reply) => {
        let helper = self.copy.as_mut().expect("copy armed");
        match helper.advance(
            FileCopyProgress::Write(reply),
            Msg::ReadCopied,
            Msg::WriteCopied,
        ) {
            FileCopyStep::Pending(effect) => effect,
            FileCopyStep::Done(report) => self.finish_copy(report),
        }
    }
}
```

The helper keeps the offsets and cap accounting. The service still gets
one continuation per rail completion, so trace and pressure stay visible.

## Related

- [`docs/mailbox-capacity.md`](mailbox-capacity.md) — every loop
  continuation consumes mailbox capacity.
- [`docs/tina-user-guide/12-io-model.md`](tina-user-guide/12-io-model.md)
  — where these helpers sit in Tina's I/O stack.
