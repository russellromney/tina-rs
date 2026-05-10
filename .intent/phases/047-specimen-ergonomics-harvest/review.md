# Phase 047 Review

## Plan Review 1

The 047 plan was already authored before this implementation cycle. The
implementation proceeded through nine commits (one import + eight
implementation slices, then a closeout sweep). Each implementation slice
landed primitive(s) plus tests, then later applied the primitive(s) to
one Specimen example as proof; the closeout commit fans the primitives
across the remaining nine examples.

Two clarifications shaped the scope during execution:

- **Sequencing.** "One end-to-end slice first" against
  `specimen_mini_keyspace` proved the harvest pattern early
  (DefaultMailboxFactory + stable_hash + BoundAddressWaiter all together)
  before fanning out across primitives.
- **Single-example proof per primitive.** Each new primitive was first
  applied to one comparison only, deferring the full sweep to the
  closeout. This kept individual commits small and reviewable.

## Implementation Review 1

### What landed

- **Rock 1 — Default mailbox factories.** `DefaultMailboxFactory` (single-
  thread) and `DefaultThreadedMailboxFactory` (Send + 'static) ship in
  `tina-runtime`. Capacity stays explicit at registration; FIFO and
  `Full` / `Closed` semantics match the trait contract; close is
  idempotent. 6 unit tests (`tina-runtime/src/tests.rs`).

- **Rock 2 — Mailbox capacity truth.** `docs/mailbox-capacity.md` names
  the rule that runtime-call replies, isolate-call replies, and
  observed-send replies all land in the requester's mailbox, plus a
  small sizing table for listener / connection / store / worker /
  fanout. 2 integration tests (`tina-runtime/tests/capacity_truth.rs`)
  pin the
  `RuntimeEventKind::CallCompletionRejected { MailboxFull, kind }` event
  for `Sleep` and `ObservedSend`.

- **Rock 3 — Stable trace fingerprint.** `RuntimeEvent::stable_hash`
  and `stable_trace_hash` ship as a hand-rolled FNV-1a walk over an
  explicit per-variant byte tag in `tina-runtime/src/trace.rs`. The
  `format!("{event:?}").hash(...)` pattern in `specimen_replay_dst` is
  gone. 5 unit tests pin determinism and variant disambiguation.

- **Rock 4 — Host observation handles.** Four typed waiters in
  `tina-runtime/src/observation.rs`: `BoundAddressWaiter`,
  `IsolateCompleteWaiter`, `OperationDoneWaiter`, and
  `ChildRestartedWaiter`. Each is bounded one-slot; the runtime drains
  the matching slot when the trace event fires. `WaitError` carries
  `Timeout` / `RuntimeStopped` / `CallFailed` / `ObservationFull`.
  `ChildRestarted` is `#[non_exhaustive]`. The `notify_child_restarted`
  hook fires after the bootstrap message has been enqueued so a host
  that wakes from `wait()` cannot race a `try_send` ahead of bootstrap
  delivery. 11 integration tests in `tina-runtime/tests/observation.rs`.

- **Rock 5 — Single-shard easy path.** `tina::SingleShard` (built-in
  `Shard` impl returning `ShardId(0)`) re-exported from `tina::prelude`.
  Both `#[tina::isolate]` and `#[tina_runtime::isolate]` default
  `shard = ::tina::SingleShard` when the argument is omitted.
  `RuntimeCallable` is a sealed marker trait with
  `#[diagnostic::on_unimplemented]` that gives a clear compile error
  pointing to `#[tina_runtime::isolate]` when an `Infallible`-Call
  isolate is registered with the simulator. 3 integration tests + 2
  compile_fail doctests.

- **Rock 6 — Sequence + TCP loop docs.** `tina::sequence(...)` is
  documented sugar for ordered effect lists (synonym of `batch`).
  `Effect::Batch` docstring names the same-stream caveat that wedged
  `specimen_mux_client`. `docs/tcp-loops.md` ships canonical write-all
  and read-to-eof patterns. Driver-level `tcp_write_all` /
  `tcp_read_to_eof` deferred — a runtime helper that hides the loop
  also hides the per-step trace event. 4 unit tests on `sequence`.

- **Rock 7 — Bridge lifecycle.** `BridgeHost::drain_and_shutdown`,
  `pending_handles()`, and `BridgeShutdownReport` ship in
  `tina-tokio-bridge`. The `Arc::try_unwrap` shutdown dance is gone
  from `specimen_axum_counter` and `specimen_ws_room`.
  `docs/bridge-composition.md` names the two-runtime shape, the
  sync-recv-inside-`block_on` deadlock, the drain pattern, and the
  signal-handler coexistence caveat. 4 integration tests.

- **Rock 8 — Runtime surface alignment.** `Runtime::try_supervise` and
  `ThreadedRuntime::try_supervise` ship as non-panicking variants that
  surface unknown / stale parents as `SuperviseError::UnknownParent`
  without crashing the worker. The panicking `supervise` is preserved
  for setup-time assertions on both. `ThreadedRuntime::try_send`'s
  fire-and-forget asymmetry vs. `Runtime::try_send` is documented
  prominently with a pointer to `send_and_observe` for the strict
  message-recoverable path. 4 integration tests.

### Closeout fan-out

Every Specimen comparison except the runner shells (`specimen_cpu_run`,
`specimen_mem_run`) was rewritten to use the new primitives:

- `DefaultThreadedMailboxFactory` replaces all 9 per-example
  `Rc<RefCell<VecDeque>>` factories (~50 lines × 9 = ~450 lines deleted).
- `SingleShard` replaces 9 per-example `KeyspaceShard` /
  `RoomShard` / etc. structs.
- `BoundAddressWaiter` replaces the `Arc<Mutex<Option<SocketAddr>>>`
  side channels in `specimen_mini_keyspace` and `specimen_real_io_chat`.
- `IsolateCompleteWaiter` replaces `Arc<AtomicBool>` `done` flags in
  `specimen_outbound_fetch` and `specimen_mux_client`.
- `ChildRestartedWaiter` replaces the `AtomicU64` generation counter
  in `specimen_supervised_worker`.
- `BridgeHost::drain_and_shutdown` replaces the `Arc::try_unwrap`
  dance in `specimen_axum_counter` and `specimen_ws_room`.
- `stable_trace_hash` replaces the `Debug`-string fingerprint in
  `specimen_replay_dst`.

### Code-impact numbers

`examples/specimen_*/src/comparison/tina_impl.rs`:

| State                   | Total LOC |
| ----------------------- | --------- |
| Before Phase 047        | 2691      |
| After Phase 047         | 2014      |
| Delta                   | **−677 (−25%)** |

Per-example LOC delta:

| Example                   | Before | After | Δ    |
| ------------------------- | -----: | ----: | ---: |
| specimen_axum_counter       |    188 |   119 |  −69 |
| specimen_graceful_shutdown  |    285 |   221 |  −64 |
| specimen_mini_keyspace      |    356 |   260 |  −96 |
| specimen_mux_client         |    219 |   143 |  −76 |
| specimen_outbound_fetch     |    246 |   175 |  −71 |
| specimen_persistent_counter |    366 |   303 |  −63 |
| specimen_real_io_chat       |    359 |   283 |  −76 |
| specimen_replay_dst         |    148 |   121 |  −27 |
| specimen_supervised_worker  |    313 |   246 |  −67 |
| specimen_ws_room            |    211 |   143 |  −68 |

### What is directly proved

- `cargo test --workspace` — green across the workspace
  (67 test groups).
- `cargo fmt --all -- --check` — clean.
- All ten primary Specimen `compare` modes run and produce equivalent
  Tokio / Tina outcomes.
- New tests: 11 observation, 6 mailbox factory, 5 stable_hash, 4
  surface alignment, 4 sequence sugar, 2 capacity truth, 4 bridge
  drain, 3 single-shard default + 2 compile_fail doctests for
  `RuntimeCallable`. Total 41 new tests; 0 regressions.
- Two `compile_fail` doctests prove `Infallible` is rejected by
  `RuntimeCallable` and `RuntimeCall<u32>` is accepted.

### What is not directly proved

- Per-step trace observability of driver-level `tcp_write_all` /
  `tcp_read_to_eof`. The plan's "or equivalent helper" clause is
  honored by `docs/tcp-loops.md` plus the user-side patterns; a
  driver-level helper that preserves trace-truth is left to a
  follow-up.
- Cross-platform stability of `stable_hash`. The FNV-1a walk is
  endian-stable in principle; documenting "stable for the current
  Tina version, not a long-term wire format" is the contract.
- Long-term replay-fingerprint stability across Tina patch versions.
  The hash is content-defined per the explicit per-variant tag, but
  adding a new field to an existing variant will change its
  fingerprint. The doc says so.

### What stays "what feels bad"

`examples/FINDINGS.md` keeps the following items as model truth or
deferred-by-design:

- Continuation enum growth (`CounterMsg`, `FetchMsg`).
- Tokio + Tina signal-handler coexistence (documented; no API change).
- Simulated process restart needs a fresh runtime (documented).
- Comparisons load-shedding metrics (deferred to runner-side work).
- Constraint runners are platform-asymmetric (documented).

### Risks / follow-ups for later phases

- Bridge `Arc::strong_count`–based `pending_handles()` is approximate:
  any non-bridge code that clones the `Arc<ThreadedRuntime>` will look
  like an outstanding handle to the drain loop. Document or constrain.
- Observation registry slots accumulate when waiters are dropped without
  being notified. Slot capacity is bounded; the auto-modifier added
  `WaitError::ObservationFull` for the bounded case but a long-running
  service that registers and drops waiters in a loop should be
  measured before declaring this a no-op.
- The `try_supervise` alignment is one-direction: `Runtime::supervise`
  panics, the threaded one returns Result. Symmetry between the
  *panicking* variants ("both panic on unknown") is not directly
  proved with a test on the threaded side, because the panic crosses
  the worker thread.

### Closeout artifacts

- `docs/mailbox-capacity.md`
- `docs/bridge-composition.md`
- `docs/tcp-loops.md`
- `examples/FINDINGS.md` updated; resolved papercuts moved to
  "resolved in Phase 047" notes alongside the original entries
  rather than into a new "resolved" section, so the historical record
  stays in place.

`SYSTEM.md` is not yet updated — none of Phase 047's primitives change
the runtime's intended semantics; they all surface facts the runtime
already records and ship a documented default for boilerplate the
runtime already supported. A follow-up SYSTEM.md edit can cite the new
helpers as the preferred user-level surface, but the rules in
SYSTEM.md are unchanged.
