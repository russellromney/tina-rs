# TCP Loops

Phase 047 Rock 6.

`tcp_write` may write fewer bytes than the buffer asked for, and `tcp_read`
returns one chunk at a time with a zero-byte read meaning EOF. These are
the truthful one-shot primitives — the runtime does not silently loop on
partial writes or accumulate reads. This page documents the canonical
patterns for write-all and read-to-eof at the user level until first-class
runtime helpers land.

## The same-stream batch caveat

`tina::batch(...)` (and its synonym `tina::sequence(...)`) execute the
contained effects in source order. The runtime processes them as one
handler turn each — but it does **not** serialize calls that target the
same I/O resource. Issuing two `tcp_write` calls against the same
`StreamId` inside one batch returns `CallError::ResourceBusy` for the
second call, because the first is still pending in the stream-write lane.

```rust,ignore
// WRONG — second tcp_write returns ResourceBusy
batch(vec![
    tcp_write(stream, request_a).then(MyMsg::Wrote),
    tcp_write(stream, request_b).then(MyMsg::WroteAgain),  // ResourceBusy
])
```

```rust,ignore
// RIGHT — chain the second write through a continuation message
match msg {
    MyMsg::WroteA(Ok(count)) if count >= request_a.len() => {
        tcp_write(stream, request_b).then(MyMsg::WroteB)
    }
    MyMsg::WroteA(Ok(count)) => {
        let mut remaining = request_a.clone();
        remaining.drain(..count);
        tcp_write(stream, remaining).then(MyMsg::WroteA)
    }
    // ...
}
```

## Canonical write-all loop

```rust,ignore
match msg {
    Msg::WriteAll(bytes) => {
        // Begin: track the buffer in self so partial writes can resume.
        self.pending_write = bytes;
        tcp_write(self.stream, self.pending_write.clone())
            .then(Msg::WroteChunk)
    }
    Msg::WroteChunk(Ok(count)) => {
        if count >= self.pending_write.len() {
            // Fully written; clear and continue with the next state.
            self.pending_write.clear();
            self.next_state()
        } else {
            // Partial write: drain the consumed prefix and re-arm.
            self.pending_write.drain(..count);
            tcp_write(self.stream, self.pending_write.clone())
                .then(Msg::WroteChunk)
        }
    }
    Msg::WroteChunk(Err(error)) => {
        // Typed I/O failure surfaces here; close the stream.
        self.fail(error)
    }
}
```

Two properties keep this honest:

1. The user's enum carries `pending_write: Vec<u8>` so partial writes
   resume from a known position. Hidden retries inside the runtime would
   throw away that observability.
2. Each progress step emits one `tcp_write` trace event and one
   `Msg::WroteChunk` mailbox delivery. A trace consumer can count the
   number of partial writes the OS forced.

## Canonical read-to-eof loop

```rust,ignore
match msg {
    Msg::ReadAll => {
        self.response_buf.clear();
        tcp_read(self.stream, self.max_chunk).then(Msg::ReadChunk)
    }
    Msg::ReadChunk(Ok(bytes)) => {
        if bytes.is_empty() {
            // Zero-byte read = EOF. Process accumulated buffer.
            self.finish_response()
        } else {
            self.response_buf.extend_from_slice(&bytes);
            // Bound total accumulation here if you must; otherwise the
            // peer can fill memory.
            if self.response_buf.len() > self.max_total {
                return self.fail_oversize();
            }
            tcp_read(self.stream, self.max_chunk).then(Msg::ReadChunk)
        }
    }
    Msg::ReadChunk(Err(error)) => self.fail(error),
}
```

## Why no built-in `tcp_write_all` / `tcp_read_to_eof` (yet)

A driver-level helper that hides the loop would be useful, but the
trade-off is that each progress step would no longer emit a trace event.
That breaks Tina's promise that the trace is the source of audit truth
for runtime-owned work. A future runtime could ship `tcp_write_all` /
`tcp_read_to_eof` *with* explicit per-step trace events; doing it without
that work is a regression.

Until that lands, the patterns above are the model truth. They are
verbose by design — each handler turn is one observable effect, and that
verbosity is what makes Tina's ordering and pressure visible.

## Related

- [`tina::batch`] / [`tina::sequence`] — sugar for ordered effect lists,
  with the same-stream caveat above.
- [`docs/mailbox-capacity.md`](mailbox-capacity.md) — every
  `Msg::WroteChunk` reply consumes one slot in the issuing isolate's
  mailbox. Capacity must hold the worst-case number of outstanding
  partial-write replies plus inbound traffic.

[`tina::batch`]: https://docs.rs/tina
[`tina::sequence`]: https://docs.rs/tina
