# Continuation And Pipeline Patterns

Tina handlers stay synchronous. A runtime call is *one* effect that
dispatches the call; the result comes back later as an ordinary inbox
message. So protocols that walk a fixed sequence (connect → write →
read → close) end up as a chain of message variants, one per step.

That shape is honest: every step is a trace event, every timeout is
explicit, every `Full`/`Closed`/error outcome is visible. But it can
read as ceremony. This page is the blessed shape so a new isolate
does not invent a worse one.

Grug rule:

```text
long explicit state machine okay.
short fake-async machine bad.
```

Helpers may remove repeated spelling. Helpers should not hide stages,
capacity, timeout, partial progress, or suspension points.

## Reply aliases

Each runtime call publishes one reply type. `tcp_connect` resolves to
`Result<(StreamId, SocketAddr, SocketAddr), CallError>`; `tcp_write`
resolves to `Result<usize, CallError>`. Spelling the full
`Result<..., CallError>` in every isolate enum is repetitive.

`tina_runtime` ships `pub type CallReply<T> = Result<T, CallError>;`
and one alias per call kind. The full set covers every runtime call
the crate exposes:

- TCP: `TcpBindReply`, `TcpAcceptReply`, `TcpConnectReply`,
  `TcpReadReply`, `TcpWriteReply`, `TcpListenerCloseReply`,
  `TcpStreamCloseReply`.
- UDP: `UdpBindReply`, `UdpSendToReply`, `UdpRecvFromReply`,
  `UdpCloseSocketReply`.
- TLS: `TlsConnectReply`, `TlsBindReply`, `TlsAcceptReply`,
  `TlsListenerCloseReply`, `TlsReadReply`, `TlsWriteReply`,
  `TlsCloseReply`.
- File / FS: `FileOpenReply`, `FileReadReply`, `FileWriteReply`,
  `FileFsyncReply`, `FileSizeReply`, `FileCloseReply`, `MkdirReply`,
  `PathMetadataReply`, `RenameReplaceReply`, `RemoveFileReply`,
  `ReadDirReply`, `SyncParentReply`.
- Persistence: `SnapshotCommitReply`, `SnapshotLoadReply`,
  `JournalAppendReply`, `JournalReplayReply`.
- Time / signal / DNS / process: `SleepReply`, `SignalWaitReply`,
  `DnsLookupReply`, `ProcessRunReply`.

Use them in your message enum:

```rust
enum FetchMsg {
    Begin,
    Connected(TcpConnectReply),
    Wrote(TcpWriteReply),
    Read(TcpReadReply),
    Closed(TcpStreamCloseReply),
}
```

The aliases do not hide anything. A handler still pattern-matches on
`Connected(Ok((stream, _, _)))` vs `Connected(Err(_))`. The trace
still carries one event per call.

## Pipeline pattern

A pipeline isolate walks a fixed sequence: each handler arm dispatches
the next call.

Canonical shape:

```rust
fn handle(&mut self, msg: FetchMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
    match msg {
        FetchMsg::Begin                                 => tcp_connect(self.target).then(FetchMsg::Connected),
        FetchMsg::Connected(Ok((stream, _, _)))         => self.start_write(stream),
        FetchMsg::Connected(Err(_))                     => self.next_iteration(),
        FetchMsg::Wrote(Ok(count))                      => self.handle_write_progress(count),
        FetchMsg::Wrote(Err(_))                         => self.close_or_iterate(),
        FetchMsg::Read(Ok(bytes))                       => self.handle_read_chunk(bytes),
        FetchMsg::Read(Err(_))                          => self.close_or_iterate(),
        FetchMsg::Closed(_)                             => self.next_iteration(),
    }
}
```

Rules:

- one trace event per arm — never call two runtime calls in a single
  handler turn against the same I/O resource;
- failure arms collapse to a small set of recovery helpers
  (`close_or_iterate`, `next_iteration`) on `impl Self`;
- timeouts live inside the call helpers (`tls_read`, `tls_write` take
  an explicit `Duration`); plain `tcp_*` calls are bounded by the
  isolate's own enclosing call timeout.

Do not:

- combine multiple runtime calls in one `Effect::Batch` for the same
  resource — the second one will see `CallError::ResourceBusy`;
- hide the state machine behind `async fn` — Tina handlers are not
  futures.
- add a pipeline DSL just to make Tina look like Tokio. If each stage
  has its own failure and pressure story, each stage should stay named
  in the message enum.

## Multi-turn reply through RequestContext

For a fixed linear request workflow, prefer `tina::flow!`; see
[Continuation Flows](29-continuation-flows.md). The manual pattern below is the
expanded form and is still useful when the workflow branches heavily.

When a service must reply after several turns, the caller's promise can be
carried as `RequestContext<R>`. This is the same primitive as `DeferredReply`
but the type name signals the multi-turn intent.

```rust
fn handle_call(&mut self, msg: SvcMsg, call_ctx: CallContext<'_, Self>) -> Effect<Self> {
    match msg {
        SvcMsg::Start => call_ctx
            .defer(call(self.probe, ProbeMsg, Duration::from_millis(50)))
            .then(SvcMsg::ProbeResult),
        SvcMsg::ProbeResult(_, _) => call_ctx.reject(CallRejectedReason::UnsupportedMessage),
    }
}

fn handle(&mut self, msg: SvcMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
    match msg {
        SvcMsg::Start => noop(),
        SvcMsg::ProbeResult(req, CallOutcome::Replied(ProbeReply(v))) if v >= 10 => {
            reply_to(req, SvcReply::Ready)
        }
        SvcMsg::ProbeResult(req, _) => {
            reply_to(req, SvcReply::NotReady)
        }
    }
}
```

Rules:

- `CallContext::defer(work)` consumes the caller in the first turn, same as
  `into_request_context`;
- `.reply(...)` on the deferred builder boxes the translator so the
  continuation message carries `RequestContext` back to the service;
- `reply_to` consumes the context and delivers the final reply;
- the caller timeout still governs; the runtime still records `Timeout`
  or `Closed` if the service forgets to reply.

Do not:

- use `RequestContext` when a single-turn `reply` suffices;
- hide the context in a shared state or closure — it must move through
  the message enum so every turn is a trace event.
- use ordinary `then(...)` in a call handler and expect it to preserve caller
  authority.

Expanded form:

```rust
let req = call_ctx.into_request_context();
call(self.probe, ProbeMsg, Duration::from_millis(50))
    .then_with_request(req, SvcMsg::ProbeResult)
```

## List-processing pattern

A list-processing isolate runs the same call sequence over each item
in a collection.

Canonical shape:

```rust
struct Worker { remaining: u32, target: SocketAddr, /* ... */ }

#[tina_runtime::isolate(message = WorkerMsg)]
impl Worker {
    fn handle(&mut self, msg: WorkerMsg, _ctx: &mut Context<'_, SingleShard>) -> Effect<Self> {
        match msg {
            WorkerMsg::Begin           => self.next_iteration(),
            WorkerMsg::ItemDone(reply) => { self.record(reply); self.next_iteration() }
        }
    }
}

impl Worker {
    fn next_iteration(&mut self) -> Effect<Self> {
        if self.remaining == 0 {
            stop_with(self.summary())
        } else {
            self.remaining -= 1;
            do_one_call(self.target).then(WorkerMsg::ItemDone)
        }
    }
}
```

Why this shape:

- one runtime call per iteration → one trace event per iteration;
- the host receives the summary via `observe_result::<Summary>(addr)` — no
  shared `Arc<...>` for the result;
- "stop on first error" or "skip and continue" are local choices in
  `record`, not framework decisions.

## TCP loop helpers

Three of these patterns appear over and over in TCP clients:

- write all the bytes (loop on partial writes);
- read exactly N bytes (loop on partial reads, error on early EOF);
- read until EOF (loop on partial reads, stop on empty bytes or a
  byte cap).

`tina_runtime::tcp_loops` ships small client-side state machines for
each: [`TcpWriteAll`], [`TcpReadExact`], [`TcpReadToEof`]. The handler
calls `next_effect(...)` to dispatch the first chunk, stores the
helper in the isolate's state, and on each reply calls `advance(...)`
to get the next step:

```rust
FetchMsg::Wrote(reply) => {
    let mut writer = self.state.write_all.take().expect("present");
    match writer.advance(reply, FetchMsg::Wrote) {
        LoopStep::Pending(effect) => { self.state.write_all = Some(writer); effect }
        LoopStep::Done(_) => self.start_read(),
        LoopStep::Failed(_) => self.close_or_iterate(),
    }
}
```

Each helper expands to one underlying `tcp_write` / `tcp_read` per
step, so partial progress is still a real trace event. No hidden
buffer growth: callers pass `max_len` for `TcpReadExact` and `max +
chunk` for `TcpReadToEof`.

## Anti-patterns

These are tempting and wrong:

- **Hidden retry**: a `tcp_write_with_retry` helper that loops
  internally. The retry would be invisible in the trace.
- **Multi-call effects**: `batch(vec![tcp_write(...), tcp_read(...)])`
  on the same stream. Same-stream calls return `ResourceBusy`.
- **Async wrapper**: an `async fn fetch(&mut self) -> Outcome` that
  awaits each runtime call. Tina handlers do not run inside a future.
- **Shared accumulator**: an `Arc<Mutex<Vec<_>>>` the isolate appends
  to so the host can read after stop. Use `stop_with(value)` instead.
