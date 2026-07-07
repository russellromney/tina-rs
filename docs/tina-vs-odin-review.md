# tina-rs vs Odin Tina Review

Strategy memo comparing `tina-rs` against its original inspiration, Peter
Mbanugo's Tina in Odin ([pmbanugo/tina](https://github.com/pmbanugo/tina),
and the blog post
[The Tokio/Rayon Trap and Why Async/Await Fails Concurrency](https://pmbanugo.me/blog/why-async-await-complect-concurrency)).
This is a review/strategy memo, not a change request.

## Executive summary

tina-rs is a faithful port of Odin Tina's **load-bearing idea** — the
synchronous handler that owns its state, takes one message, and returns a
closed `Effect` the runtime interprets — and it has genuinely *raised* the
ceiling on two axes Odin only gestures at: Rust's compile-time type rails, and
deterministic simulation as a first-class, replayable, shrinkable dev mode.

But it has also grown a second body around that idea. Odin Tina is one
language, one repo, one binary, with maybe a dozen nouns. tina-rs is **19
first-party crates, ~303 public types in `tina-runtime` alone, 45 runtime-call
helpers, 51 examples, 27 user-guide pages, and seven ecosystem bridges** — plus
native HTTP/2, gRPC, and WebSocket batteries and five AWS service surfaces. The
handler still feels like Tina. The framework around it now feels closer to the
Akka-scale system the ROADMAP explicitly lists as a non-goal
(`ROADMAP.md` non-goals).

Two original load-bearing simplicities did **not** survive the port, and the
docs are honest about it but the positioning is not: Odin's *arena /
no-malloc-after-boot* memory story (tina-rs makes no such claim —
`.intent/SYSTEM.md` mailbox model), and Odin's *OS-trap fault boundary that
catches segfaults* (tina-rs has Rust panic capture only — `ROADMAP.md` failure
isolation row). Also, the headline "thread-per-core scheduling" outruns the
substrate: the **semantic oracle is an explicit-step, single-threaded
multi-shard model**, not pinned parallel execution (`.intent/SYSTEM.md`
multi-shard section).

Net: the model is preserved well; the *surface* has drifted toward the breadth
Tina was a reaction against. The highest-value work before the Alpaca launch is
subtraction and honest positioning, not more features.

## Comparison table

| Dimension | Odin Tina | tina-rs | Verdict |
|---|---|---|---|
| **Core model** | Isolate = state machine, sync handler `proc(self, msg, ctx) -> Effect` | Same. `handle(&mut self, msg, ctx) -> Effect<Self>` (`README.md`) | **Preserved** |
| **Effect language** | Closed `Effect` enum (`Effect_Io`, `Effect_Done`, …) | Closed `Effect<I>`, 13 variants (`tina/src/effect.rs:26`) | Preserved; grew from documented 6 → 13 |
| **Scheduling/threading** | Real thread-per-core, OS threads pinned per CPU, strict tick, no work-stealing, no migration | Live `ThreadedRuntime` exists, but the **oracle is explicit-step multi-shard, not parallel**; no hard OS pinning claimed (`SYSTEM.md`) | **Diverged / weaker** |
| **I/O model** | Per-shard reactor: kqueue / io_uring / IOCP | Runtime-owned rails over vendored **Betelgeuse** (io_uring/kqueue/sim); TCP, TLS (rustls sans-I/O), **and now Unix-domain sockets** ride the substrate on the shard thread, leaving DNS/process as justified blocking lanes and a narrow rename/remove/readdir/metadata storage fallback; rails self-classify in the capability report and a static guard blocks new bypass lanes; 45 call helpers | Preserved shape, different substrate |
| **State ownership** | Generational handles into dense per-type arenas; never raw pointers | Typed `Address<M,R>` w/ shard+id+generation; **but `Arc<Mutex>` escape hatch compiles** (`01-mental-model.md`) | Preserved + honest caveat |
| **Memory model** | Grand/Typed/Scratch arenas; **malloc never called after boot** | **No allocation-free claim**; only narrow pinned hot paths (`SYSTEM.md`) | **Diverged / weaker** |
| **Backpressure** | Bounded mailboxes, drop-on-full (README) / "caller notified immediately" (blog) | Typed `Full`/`Closed`/`Timeout` outcomes everywhere (`README.md`) | **tina-rs better** |
| **Fault isolation** | OS trap boundary catches crashes *and segfaults*; supervisor restarts; shard quarantine | Rust panic capture only; "**not Tina-Odin's OS trap boundary**" (`ROADMAP.md`); quarantine deferred | **Odin stronger** |
| **DST / replay** | Swap effect interpreter; same seed+config = same execution | `tina-sim` + live-trace→replay capture, `ReplayCase` bug-in-a-box, deletion shrinking, protocol facts (`ROADMAP.md`) | **tina-rs better** |
| **Type safety** | Runtime generational handles | Compile-time: typed addresses, `Send` bounds, `compile_fail` proofs, handle/call & event/request split (`21-compile-time-safety-rails.md`) | **tina-rs better** |
| **Ecosystem** | None — pure framework | 7 bridges + HTTP/1.1/2, gRPC, WebSocket, AWS×5 (`README.md`) | **tina-rs broader (and a risk)** |
| **Surface size** | ~1 language, docs/concepts handful | 19 crates, 303 runtime types, 87 core types, 27 doc pages | **Odin simpler** |
| **Maturity/language** | Odin, "Early but Functionally Stable", macOS-primary | Rust, "experimental, API still moving", make-verify gate | Comparable |

## Findings (ordered by importance)

### 1. Core idea preserved — and it's the right one (Keep)

The thing that makes Tina *Tina* is intact. `tina/src/effect.rs:26` is a closed
`Effect<I>` enum; handlers are synchronous and return one effect (`README.md`);
runtime calls come back as ordinary message variants via `.then(...)`. The
README's TCP echo is recognizably the same program as Odin's `echo_handler`.
The porting guide's await→message-variant mapping
(`docs/tina-user-guide/09-tokio-to-tina-porting.md`) is exactly the blog's
thesis made mechanical. This is faithful, and the `#[must_use]` on `Effect`
is a nice Rust-native enforcement of "a handler must say what happens next."

### 2. API/noun overload is the dominant drift (Change — highest severity)

The README promises "copyable local patterns are safer than clever APIs whose
important rules live somewhere else." The numbers cut against it:
**`tina-runtime` exposes ~303 public structs/enums; `tina` core exposes 87.**
The clearest symptom is the bounded-pending-helper family. A user holding "I
have outstanding work to remember" must currently choose among
**`PendingReplies`, `PendingCallSet`, `PendingCancelableCallSet`,
`CancelableWork`, `SharedWork`, `SharedWork`, `GuardedPendingReplies`** — and the
ROADMAP itself ends with a four-way decision tree for which to pick. FINDINGS
documents the same fragmentation in admission: a 7-variant `AdmissionDecision`,
plus the `ConcurrencyPermit`-doesn't-drop-but-`SharedLease`-does mismatch that
pushed a whole specimen *off* the blessed helper
(`examples/FINDINGS.md`). This is helper sprawl arriving exactly where the
project said it wouldn't.

### 3. Battery sprawl pulls toward the Akka non-goal (Change)

`ROADMAP.md` lists "Akka feature parity" as a non-goal. Yet the batteries now
include native HTTP/1.1, HTTP/2 (with full wire-error taxonomy), gRPC h2c
(unary/server/client/bidi, tonic interop), a WebSocket server with subprotocols
and slow-peer eviction, and an AWS bridge covering **S3, SQS, SNS, DynamoDB, and
Secrets Manager** (`README.md`). `docs/tina-user-guide/23-core-and-batteries.md`
does good defensive work drawing the core/battery line, but the existence of
that doc is itself evidence the surface got big enough to need policing. Every
battery is real maintenance and real "learn Tina without HTTP" risk.

### 4. Two original simplicities were lost, and positioning hides it (Change)

- **Memory.** Odin's "no malloc after boot" is load-bearing simplicity and a
  determinism claim. tina-rs explicitly *cannot* make it: "boxed erasure, call
  translators, trace storage, replay records, completion slots… may allocate"
  (`SYSTEM.md`). Correct and honest — but no user-facing page leads with "we did
  not port the arena guarantee."
- **Fault isolation.** Odin catches segfaults at an OS trap boundary and
  restarts the isolate. tina-rs has unwinding-panic capture only and says so
  internally (`ROADMAP.md`), but the README's supervision prose reads stronger
  than the substrate is.

### 5. The "thread-per-core" headline outruns the substrate (Change)

`README.md` and the SYSTEM vision sell "thread-per-core scheduling." But
`SYSTEM.md` is explicit that the **multi-shard runner is an explicit-step model,
not real parallel execution**, and the live `ThreadedRuntime` disclaims "hard OS
thread pinning, peer quarantine, cross-shard child ownership." Odin's strength
is the inverse: the pinned-thread substrate *is* the product. Here the proven,
canonical thing is the deterministic explicit-step oracle, and the parallel
substrate is narrower than the headline implies.

### 6. Bridges are seven hidden-queue attack surfaces (Defer/monitor)

Each bridge (`tokio`, `tower`, `reqwest`, `sqlite`, `sqlx`, `aws`, plus
`rpc-tokio`) is a place where Tina's bounded story meets an SDK's internal
queues/threads. The project polices this well —
`docs/tina-user-guide/23-core-and-batteries.md` lists six battery rules, and
`SYSTEM.md` insists "the bridge is not the main runtime story" — but the
FINDINGS AWS section shows the cost: "five services share roughly 80% of their
lifecycle code… copy-pasted plumbing." The discipline is holding; the surface is
large enough that one slip leaks an unbounded queue.

### 7. Effect enum growth is mild but worth a look (Change, minor)

Documented as 6 user verbs (`README.md`), the live enum is 13 variants:
`Noop, Reply, Reject, Send, Spawn, SpawnObserved, Stop, StopWith,
RestartChildren, Call, Batch, …` (`tina/src/effect.rs`). Still closed — the core
discipline holds — but `Reject`, `SpawnObserved`, `StopWith`, `RestartChildren`
are arguably refinements that could be expressed without top-level variants.
Watch this, don't panic.

### 8. Where tina-rs is unambiguously more ambitious (Keep)

- **Replay/DST beyond Odin.** Live trace → replay capture,
  `ReplayCase`/`ReplayReport` "bug in a box", deletion shrinking, seed sweeps,
  and **protocol facts** (HTTP/2/WebSocket/gRPC wire outcomes replayable)
  (`ROADMAP.md`, `examples/FINDINGS.md`). Odin states "same seed = same
  execution"; tina-rs makes the failing run a portable, shrinkable artifact.
  The honest line, now stated in the docs and `.intent/SYSTEM.md`: this replay
  is of **logical** interleavings. The simulator is single-threaded on purpose
  and does not catch physical memory-ordering races — those live on a small,
  enumerated shared-memory surface (the SPSC mailbox and `SharedCapacityScope`)
  that loom checks instead. Replay is not "all the way down" to the physical
  substrate, and the positioning no longer implies it is.
- **Compile-time rails.** Typed `Address<M,R>`, `Send`-bound enforcement, the
  four `compile_fail` leak proofs (`01-mental-model.md`), and the event/request
  split that makes "request variant matched in event handler" a compile error
  (`examples/FINDINGS.md`). Rust buys static guarantees Odin gets only at
  runtime.
- **Honest capability reporting.** `RuntimeCapabilities` with
  `NotClaimed`/`Unsupported`/`PollBacked`/`Tombstoned` shapes (`SYSTEM.md`) is a
  maturity Odin's README doesn't show.
- **Typed overload vocabulary** (`Full`/`Closed`/`Timeout`/`CommitUncertain`) is
  strictly richer than Odin's drop-on-full.

## Recommendations

1. **Consolidate the bounded-pending / admission helper family.** *(API
   ergonomics)* This is the #1 threat to the "copyable local pattern" promise.
   The decision tree already exists (`ROADMAP.md`); turn it into either one
   generic type with type-params or a single one-page chooser, and deprecate the
   overlap. Do this before the Alpaca rename so the launch surface is the
   trimmed one.

2. **Freeze the battery and AWS surface; gate new bridges on evidence.**
   *(Roadmap positioning)* Declare HTTP/1.1+2 / gRPC / WebSocket "enough," stop
   expanding AWS service coverage past the current five, and move "more
   bridges/smol" to explicitly evidence-gated. Re-state the Akka non-goal in
   launch-facing prose, not just `ROADMAP.md`.

3. **Write one "Coming from Odin Tina" honesty page.** *(Docs-only)* State
   plainly: no arena/no-malloc-after-boot guarantee; no OS-trap/segfault
   isolation (panic capture only); multi-shard oracle is explicit-step, not
   pinned-parallel; `Arc<Mutex>` escape hatch compiles. This is a credibility
   asset and inoculates against over-claiming at launch. Cheap, high-trust.

4. **Reconcile the parallelism headline with the substrate — pick one.**
   *(Runtime/core or positioning)* Either invest to make pinned thread-per-core
   the real headline (close pinning / peer-quarantine / shard-restart) **or**
   downgrade the pitch from "thread-per-core scheduling" to "shard-local bounded
   actor model with a deterministic explicit-step oracle." Today the strongest,
   most-proven artifact (the DST oracle) is undersold and the least-proven
   (parallel substrate) is oversold.

5. **Re-anchor every doc and every new noun on the handler.** *(API ergonomics
   + docs)* The one thing that is excellent and unambiguously Tina is the
   synchronous `handle` returning a closed `Effect`. Lead with it, and make
   "does this keep suspension / failure / capacity visible?" the explicit gate
   for any new public noun. The FINDINGS file is already doing this informally
   (e.g. the deliberate refusal to build a pipeline helper) — make it a formal
   noun budget.

## Keep / Change / Defer

**Keep**

- Synchronous handler + closed `Effect<I>` (`tina/src/effect.rs:26`) — the soul
  of the port.
- Typed `Full`/`Closed`/`Timeout` backpressure — richer than Odin's
  drop-on-full.
- `tina-sim` as separate semantic oracle + `ReplayCase`/shrinking/protocol facts
  — *better* than Odin.
- Compile-time rails (typed addresses, `Send` bounds, `compile_fail` proofs,
  event/request split).
- `RuntimeCapabilities` / `NotClaimed` honesty discipline.
- `.intent/SYSTEM.md` as a shape-protection contract.

**Change**

- Collapse the pending/wait/admission helper family (Finding 2).
- Freeze/trim batteries + AWS surface (Finding 3).
- Add the Odin-divergence honesty doc (Finding 4).
- Reconcile "thread-per-core" headline with explicit-step reality (Finding 5).
- Audit `Effect` variant growth — fold `Reject`/`SpawnObserved`/`StopWith` if
  they don't need top-level status (Finding 7).

**Defer**

- Real pinned-parallel substrate hardening (peer quarantine, shard restart, OS
  pinning) — only if the launch pitch commits to parallelism as headline.
- OS-trap / segfault isolation — out of scope for safe Rust; document as an
  explicit non-goal rather than chase Odin here.
- More bridges (smol, etc.), `join-all`/`stream-select`, the `flow!` surface —
  all already correctly deferred in `ROADMAP.md`; keep them deferred.

## Sourcing note

Odin claims are from the [pmbanugo/tina README](https://github.com/pmbanugo/tina)
(Programming Model, I/O, State Ownership, Scheduling, Supervision, DST sections)
and the [blog post](https://pmbanugo.me/blog/why-async-await-complect-concurrency)
(the easy-vs-simple framing, the Tokio/Rayon "human in the loop scheduler" trap,
"Predictability beats brevity"). One internal tension worth flagging: the README
says backpressure is **drop-on-full** while the blog says the **caller is
notified immediately** — tina-rs's typed-outcome model (`Full`/`Closed`)
resolves that ambiguity in the better direction. tina-rs claims are cited to
local files/line ranges throughout.
