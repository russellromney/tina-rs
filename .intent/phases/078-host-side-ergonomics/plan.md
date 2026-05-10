# 078 Host-Side Ergonomics

## Status

- Done: plan created after `ThreadedRuntime::call_blocking` proved useful
  in HTTP and DB specimens.
- In progress: none.
- Open: implement host/test helpers.
- Deferred: service-handler syntax sugar, pipeline macros, fake async.

## Goal

Host code is not service code.

Tests, specimens, and examples often need:

```text
host asks isolate one question.
host waits for answer.
host inspects trace.
host drives a short scenario.
```

Today that sometimes grows a fake Driver isolate. That is noise.

This phase reduces host/test ceremony without changing Tina service
truth. Inside service handlers, ordinary messages and `Effect` remain
the model.

## Non-Goals

- No `await`-like service syntax.
- No hidden service state machine.
- No unbounded host queue.
- No helper that is usable from inside `handle()` if it would block.
- No helper that erases `Full`, `Closed`, `Timeout`, or typed reply
  shape.

## Rock 0 — Audit Current Host Scripts

Read specimens/tests that recently paid host ceremony:

- `specimen_outbound_http`;
- `specimen_sqlite_counter`;
- `specimen_postgres_counter`;
- native HTTPS tests/specimen;
- HTTP keepalive tests;
- any test with a one-off Driver isolate whose only job is "Begin /
  Returned / send to channel".

Classify each Driver:

- **fake host driver**: convert or document conversion path;
- **real service state machine**: leave it alone.

## Rock 1 — Document `call_blocking`

Make the copied path obvious:

```rust
let outcome = runtime.call_blocking(addr, Msg::Request(x), timeout)?;
match outcome {
    CallOutcome::Replied(reply) => { ... }
    CallOutcome::Full => { ... }
    CallOutcome::Closed => { ... }
    CallOutcome::Timeout => { ... }
}
```

Docs must say:

- host/test only;
- never call from an isolate handler;
- preserves `CallOutcome`;
- good for scripts/specimens;
- services should use `call(...).reply(...)`.

Update examples where the fake host Driver is still present.

## Rock 2 — Trace Query Helpers

Tests repeat `trace.iter().filter(matches!(...)).count()`.

Add small read-only helpers near `RuntimeEvent` / trace snapshot:

```rust
trace.count_completed(CallKind::TcpAccept)
trace.count_failed(CallKind::TlsBind)
trace.any_rejected(...)
trace.find_call(...)
```

Keep it boring:

- no query language;
- no allocation-heavy index;
- no matcher framework unless two helpers prove insufficient;
- helpers return counts/options over existing trace slices.

Use them in HTTP/TLS/keepalive tests that currently hand-roll the same
filter.

## Rock 3 — Tiny Scenario Runner, Only If Pulled

Only build this if Rock 0 finds three tests with the same shape:

```rust
scenario(&runtime, addr)
    .send(Msg::Begin)
    .sleep(Duration::from_millis(20))
    .send(Msg::Finish)
    .run()?;
```

Rules:

- test/specimen helper only;
- explicit sleeps only;
- no "wait until idle" lie;
- no service-handler API.

If not clearly pulled, document the pattern and stop.

## Done Means

- Fake host Driver patterns are reduced where safe.
- Docs clearly separate host `call_blocking` from service
  `call(...).reply(...)`.
- Trace query helpers remove repeated match/filter code in real tests.
- No service-state-machine truth is hidden.
- Roadmap/changelog updated.
