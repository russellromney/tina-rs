# Plan Review: Baobab Production-Readiness Rails

Verdict: right phase, not yet implementation-tight.

Baobab is the correct next hard gate after Blue Whale. It should not add a big
new product story. It should prove whether the local Tina shape is ready for
serious small-service port attempts: visible overload, traceable failure,
replayable races, bounded live rails, and honest comparison rows.

What is strong:

- The goal is user-shaped readiness, not README theater.
- The non-goals are sharp: no remoting, clustering, durable mailbox,
  Tower/Axum-inside-Tina, or broad performance claim.
- The phase asks for capability truth before comparison claims.
- The service gauntlet, bridge gauntlet, DST gauntlet, and backpressure wall
  are the right families of proof.
- Blue Whale's advisory affinity truth is carried forward instead of pretending
  hard pinning exists.

Load-bearing fixes before implementation:

1. **Portable-completion dependency must stay explicit.**
   Baobab should run after the portable local runtime is completed, not after
   `io_uring`. Pin the rule: Baobab judges landed backend truth. If `io_uring`
   has not landed, Baobab records portable-backend rows only. If it has landed,
   Baobab includes it without changing Tina user semantics.

2. **Glommio comparison must be platform-gated.**
   Glommio is not a portable baseline for every CI/dev box. Add a required
   unsupported/skipped row for non-Linux or unsupported environments. Do not add
   a hard workspace dependency that breaks macOS verify. Use feature-gated or
   separate comparison code.

3. **Capability matrix needs one executable source of truth.**
   Do not make a Markdown table plus tests that drift. Recommended shape:
   `tests/readiness_matrix.rs` owns typed rows with `Supported | Partial |
   Unsupported | NotClaimed | PlatformGated`, expected Tina capability data,
   and optional Tokio/Glommio comparison notes. Review/changelog summarize it.

4. **The service gauntlet is too easy to make flaky if every rail is in one
   monster test.**
   Pin the shape: one composed user service proves the happy cross-rail path,
   then focused negative/overload/shutdown tests prove each rail's scary edge.
   Do not require DNS, TLS, process, file, persistence, cross-shard, and signal
   to all fail in one test.

5. **DST requirements are too vague.**
   "Combine at least three rails" is good, but the plan must name the first
   seed families and their invariants. Examples: late completion after
   requester stop, pressure then shard failure, persistence corruption then
   restart, bridge cancellation then retry. Each family needs saved seed,
   replay assertion, and shrink/minimize expectation if the harness supports it.

6. **"Shrink at least one failing-style history" may overclaim current tools.**
   Stuga has history-as-data and deletion shrinking, but not every new DST
   family may shrink meaningfully. Pin: at least one Baobab DST family must
   exercise the shrinker; the rest must have saved seeds and deterministic
   replay.

7. **Bridge gauntlet must pin timeout semantics.**
   The bridge already chose "caller timeout/cancel does not mean Tina work was
   never submitted unless cancellation wins before admission." Baobab should
   test and name this exact behavior so porters do not mistake timeout for
   rollback.

8. **Cost numbers need a no-claim contract.**
   Add a tiny report command, not benchmark mythology. It must print environment,
   backend, build profile, row status, and numbers. No CI threshold except "the
   command runs." No "faster/slower" wording in artifacts unless a later phase
   designs benchmark policy.

9. **CI rails already exist and should be extended, not reinvented.**
   `.github/workflows/verify.yml` runs `make verify` on Ubuntu and macOS. Baobab
   should add named jobs/commands only where needed: readiness matrix, selected
   DST seeds, and optional platform-gated comparison jobs. Keep slow and
   host-specific tests out of default `make verify` unless they are stable.

10. **API sweep needs a decision rule.**
   "Fix footguns" can become ergonomics sprawl. Pin it to names/helpers used by
   the gauntlet that directly affect safety or visibility: hidden queues,
   ambiguous timeout/cancel names, missing report methods, missing capability
   truth. No broad macro/flow work.

11. **Backpressure wall should enumerate exact pressure sources.**
   The plan names families, but implementation needs exact rows: mailbox full,
   live ingress full, live shard-pair full, bridge ingress full, storage lane
   full, DNS/TLS/process lane full, signal capacity full, persistence append
   rejection, and requester completion mailbox full where relevant.

12. **Readiness report must distinguish "runs" from "replacement."**
   Baobab can say Tina has a serious local-service readiness gate. It should
   not say Tina is a general Tokio replacement, production ready, or Glommio
   performance peer unless future phases earn those claims.

Recommended plan edits:

- Add a "Backend Comparison Rule" section with portable backend, optional
  North Sea, Tokio, and platform-gated Glommio rows.
- Add a "Matrix Source Of Truth" section naming the Rust test/table as the
  authority.
- Split Service Gauntlet into "one composed happy service" and "focused scary
  edge tests."
- Add named DST seed families and exact invariants.
- Change Required Proof from "shrink at least one failing-style history" to
  "at least one new DST family exercises deletion shrinking; all new families
  have saved seeds and replay checks."
- Add the bridge timeout/cancel semantic as an explicit required test.
- Add "cost command runs, reports environment/backend/profile, no thresholds."
- Extend CI from existing `.github/workflows/verify.yml`; do not replace it.

After those edits, Baobab is ready to execute as a hard production-readiness
gate.
