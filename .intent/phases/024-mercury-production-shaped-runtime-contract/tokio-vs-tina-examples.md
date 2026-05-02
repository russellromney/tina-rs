# Tokio vs Tina Comparison Examples

Purpose: pin the Mercury comparison suite before building it.

These examples are not marketing copy. They are test candidates. Each one asks:

- Does Tokio behavior match the expected shape?
- Does Tina behavior match the expected shape?
- Is Tina close enough in user code, while making overload/timeouts/replay more
  explicit?

The comparison must include naive Tokio where useful, hardened Tokio where
fair, and Tina using normal primitives without special app-level tuning.

## Scoring

- **Ease A:** about as easy as Tokio.
- **Ease B:** a little more ceremony, but clearer failure semantics.
- **Ease C:** noticeably more ceremony; keep only if the safety proof matters.

## The 20 Examples

### 1. Fire-and-forget local send

Tokio sketch:

```rust
tx.send(msg).await?;
```

Tina sketch:

```rust
send(worker, Msg::Run(job))
```

Expected behavior: accepted sends arrive later in FIFO order. If the queue is
closed/full, ordinary Tina `send(...)` records trace but does not report to the
sender. This matches the simple Tokio happy path.

Verdict target: **Ease A**. Tina should feel at least as simple.

### 2. Observed bounded send

Tokio sketch:

```rust
match tx.try_send(msg) {
    Ok(()) => accepted += 1,
    Err(TrySendError::Full(_)) => rejected += 1,
    Err(TrySendError::Closed(_)) => closed += 1,
}
```

Tina sketch:

```rust
send_observed(worker, Msg::Run(job)).reply(SessionMsg::SendObserved)
```

Expected behavior: both expose `Accepted` / `Full` / `Closed`. Tina returns the
outcome as an ordinary later message, preserving effect-returning handlers.

Verdict target: **Ease B**. Slightly more ceremony, better handler discipline.

### 3. Request/reply with timeout

Tokio sketch:

```rust
let (reply_tx, reply_rx) = oneshot::channel();
tx.try_send(Work { job, reply_tx })?;
let result = timeout(Duration::from_millis(50), reply_rx).await;
```

Tina sketch:

```rust
call(worker, WorkRequest::Run(job), Duration::from_millis(50))
    .reply(SessionMsg::WorkReturned)
```

Expected behavior: caller receives exactly one completion: reply, target full,
target closed, or timeout.

Verdict target: **Ease A/B**. Tina should be shorter and more defined.

### 4. Target mailbox full on request/reply

Tokio sketch:

```rust
let (reply_tx, reply_rx) = oneshot::channel();
match tx.try_send(Work { job, reply_tx }) {
    Err(TrySendError::Full(_)) => WorkOutcome::Full,
    other => wait_for_reply_or_timeout(other, reply_rx).await,
}
```

Tina sketch:

```rust
call(worker, WorkRequest::Run(job), timeout).reply(SessionMsg::WorkReturned)
```

Expected behavior: Tina yields `CallOutcome::Full`; hardened Tokio must remember
to use `try_send` and map the failure before waiting on the reply.

Verdict target: **Ease A** for Tina; **Tokio hardened required**.

### 5. Target closed/stale on request/reply

Tokio sketch:

```rust
if tx.try_send(req).is_err() {
    WorkOutcome::Closed
}
```

Tina sketch:

```rust
call(worker_addr, req, timeout).reply(SessionMsg::WorkReturned)
```

Expected behavior: Tina yields `CallOutcome::Closed` for stopped/stale target.
Trace contains target identity. Tokio only knows channel closed unless the app
adds generation/identity metadata.

Verdict target: **Ease A/B**. Tina is more explicit about stale identity.

### 6. Requester stops before reply

Tokio sketch:

```rust
drop(reply_rx); // worker later sees reply_tx.send(...).is_err()
```

Tina sketch:

```rust
batch(vec![
    call(worker, req, timeout).reply(SessionMsg::WorkReturned),
    stop(),
])
```

Expected behavior: Tina trace records `CallCompletionRejected {
RequesterClosed }`; no user message is delivered. Tokio needs explicit app
logging to make this visible.

Verdict target: **Ease B**. Tina gives better default observability.

### 7. Late reply after timeout

Tokio sketch:

```rust
let result = timeout(short, reply_rx).await;
// worker may later fail reply_tx.send(...)
```

Tina sketch:

```rust
call(worker, slow_req, Duration::ZERO).reply(SessionMsg::Returned)
```

Expected behavior: Tina first delivers `CallOutcome::Timeout`; a later target
`reply(...)` is observed as an ordinary reply effect, not as a second call
completion.

Verdict target: **Ease B**. Tina pins the weird edge clearly.

### 8. Requester mailbox full at completion

Tokio sketch:

```rust
// reply channel is independent of requester's main mailbox
reply_tx.send(result).ok();
```

Tina sketch:

```rust
call(worker, req, timeout).reply(SessionMsg::Returned)
// requester mailbox fills before reply is enqueued
```

Expected behavior: Tina trace records `CallCompletionRejected { MailboxFull }`.
This is stricter than many Tokio app shapes because completions use the same
bounded message path as all other isolate messages.

Verdict target: **Ease C**, but valuable proof of one-queue semantics.

### 9. Retry after bounded backoff

Tokio sketch:

```rust
loop {
    if try_work().await.is_ok() { break; }
    sleep(backoff).await;
}
```

Tina sketch:

```rust
match msg {
    Msg::Attempt => call(worker, req, timeout).reply(Msg::Returned),
    Msg::Returned(CallOutcome::Full | CallOutcome::Timeout) => {
        sleep(backoff).reply(|_| Msg::Attempt)
    }
    Msg::Returned(CallOutcome::Replied(_)) => done(),
    Msg::Returned(CallOutcome::Closed) => stop(),
}
```

Expected behavior: both retry; Tina names each failure branch as ordinary
message flow and simulator replay can pin the sequence.

Verdict target: **Ease B/C**. More verbose, much more inspectable.

### 10. TCP read EOF

Tokio sketch:

```rust
let n = stream.read(&mut buf).await?;
if n == 0 { break; }
```

Tina sketch:

```rust
tcp_read(stream, max).reply(ConnMsg::Read)
// Read(Ok(bytes)) if bytes.is_empty() => stop()
```

Expected behavior: EOF is visible as empty bytes in both. Tina uses completion
message instead of `await`.

Verdict target: **Ease B**.

### 11. Partial TCP write

Tokio sketch:

```rust
stream.write_all(&bytes).await?;
```

Tina sketch:

```rust
tcp_write(stream, bytes).reply(ConnMsg::Wrote)
// if count < pending.len(), issue another tcp_write
```

Expected behavior: Tokio `write_all` hides partial writes in helper logic;
Tina currently exposes partial writes. Tina is more explicit but more verbose.

Verdict target: **Ease C**. Candidate for future helper, not core flaw.

### 12. Bounded accept handoff

Tokio sketch:

```rust
let permit = semaphore.clone().try_acquire_owned()?;
tokio::spawn(handle_conn(stream, permit));
```

Tina sketch:

```rust
spawn(ChildDefinition::new(Connection::new(stream), capacity)
    .with_initial_message(ConnMsg::Start))
```

Expected behavior: both bound active connections. Tokio requires an explicit
semaphore; Tina bounds through isolate/mailbox capacities and spawn policy.

Verdict target: **Ease B**.

### 13. Worker pool overload

Tokio sketch:

```rust
match worker_tx.try_send(job) {
    Ok(()) => accepted += 1,
    Err(Full(_)) => shed += 1,
    Err(Closed(_)) => restart_or_fail(),
}
```

Tina sketch:

```rust
send_observed(worker, WorkMsg::Run(job)).reply(SessionMsg::Observed)
```

Expected behavior: both can shed overload; Tina keeps overload result in the
same isolate message loop and trace.

Verdict target: **Ease A/B**.

### 14. Worker crash and restart

Tokio sketch:

```rust
let join = tokio::spawn(worker_loop(rx));
if join.await.is_err() {
    restart_worker();
}
```

Tina sketch:

```rust
runtime.supervise(parent, SupervisorConfig::new(RestartPolicy::OneForOne, budget));
```

Expected behavior: both can restart, but Tokio app must own restart policy,
budget, and stale sender cleanup. Tina has direct-child supervision semantics
and trace events.

Verdict target: **Ease A/B** for Tina on supervised workloads.

### 15. Stale worker address after restart

Tokio sketch:

```rust
// app must replace old Sender and ensure stale clones are not used
```

Tina sketch:

```rust
send_observed(old_worker_addr, msg).reply(SessionMsg::Observed)
```

Expected behavior: Tina returns/records `Closed` for stale generation. Tokio
needs app-managed generation or routing indirection to detect this precisely.

Verdict target: **Ease A** for Tina safety.

### 16. Cross-shard bounded transport full

Tokio sketch:

```rust
source_to_dest_tx.try_send(msg)
```

Tina sketch:

```rust
send_observed(remote_worker, msg).reply(SessionMsg::Observed)
```

Expected behavior: both can model bounded transport if Tokio app builds a
source-destination queue. Tina multi-shard runtime has shard-pair capacity in
the coordinator.

Verdict target: **Ease B**. Tina should make this harder to forget.

### 17. Deterministic timer replay

Tokio sketch:

```rust
tokio::time::pause();
tokio::time::advance(duration).await;
```

Tina sketch:

```rust
let artifact = sim.replay_artifact();
let replayed = run(artifact.config().clone());
assert_eq!(artifact.event_record(), replayed.event_record());
```

Expected behavior: Tokio can test time with paused time, but whole app replay
requires extra harness discipline. Tina simulator records event-level replay.

Verdict target: **Ease B**. Tina wins on whole-workload proof.

### 18. Seeded local-send perturbation

Tokio sketch:

```rust
// build test doubles / randomized delays around channels
```

Tina sketch:

```rust
SimulatorConfig {
    seed,
    faults: FaultConfig { local_send: DelayBy { one_in, steps }, .. },
    ..
}
```

Expected behavior: Tina should reproduce the same failure under the same seed
and diverge under selected different seeds. Tokio needs app-specific fault
injection.

Verdict target: **Ease A** for Tina testing.

### 19. Live bounded ingress to worker thread

Tokio sketch:

```rust
runtime_tx.try_send(Command::Ingress(msg))
```

Tina sketch:

```rust
threaded.try_send(addr, msg)
```

Expected behavior: both report bounded handoff full without waiting for worker
handler completion. Tina already has a direct proof for `IngressFull`.

Verdict target: **Ease A**.

### 20. Constrained overload service

Tokio sketch:

```rust
// naive: spawn per request + unbounded or large queues
// hardened: bounded mpsc + semaphore + timeout + cancellation + metrics
```

Tina sketch:

```rust
// sessions call workers with mandatory timeout
// overload uses CallOutcome / SendOutcome
// simulator replays the same state-machine failure shape
```

Expected behavior:

- naive Tokio accepts too much work, grows memory/latency, or times out late
- hardened Tokio survives with explicit backpressure discipline
- Tina survives with normal bounded primitive use

Verdict target: **Ease B**. Tina should be close to hardened Tokio in code size,
but safer by default and easier to replay.

## Expected Overall Result

Tina should not beat Tokio on every small syntax example. Tokio should win on
raw async I/O ecosystem familiarity and helpers like `write_all`.

Tina should win, or be close enough, on the examples where failures matter:

- full queues
- closed/stale targets
- mandatory timeout request/reply
- requester stopped before completion
- supervision restart
- deterministic replay
- seeded perturbation
- visible overload recovery

If the comparison suite shows Tina is only safer by adding much more user code,
Mercury should pause and add small helpers rather than push the claim.

If the suite shows Tina is close to hardened Tokio while requiring less
discipline to avoid hidden queues/timeouts, Mercury has the demo it needs.

