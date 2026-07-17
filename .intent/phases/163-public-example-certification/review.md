# Reviews

## Execution Review 1

**Disposition: revise before implementation.**

The intent and broad proof categories are sound, but the first execution draft
still required the implementer to reconstruct scope and make product decisions.
The following changes are required before implementation:

1. Freeze the exact `origin/main` SHA and reconcile #364, #365, and #366 from
   durable PR evidence. Agent-chat recollection is not proof.
2. Enumerate the complete public corpus and give every target one disposition:
   one migration owner, intentional allowlist, or already canonical with proof.
3. Give each target exactly one owning migration PR. In particular, do not
   revisit job queue, metrics shipper, WebSocket room, or a production host in
   several cohorts.
4. State the PR dependency graph, safe concurrency, worktree ownership, rebase
   order, and durable resume checkpoint. Do not concurrently edit a shared
   findings ledger.
5. Settle typed child-lifecycle authority, capacity, failure, cancellation,
   duplicate, stale-generation, trace, and live/simulator semantics before code.
6. Settle keepalive installation atomicity, partial rollback, duplicate install,
   close, timeout retention, owner failure, and shutdown semantics before code.
7. Separate HTTP and WebSocket prerequisites. Precisely define the WebSocket
   correction's recipients, ordering, exactly-once boundary, closure behavior,
   and call/send authority.
8. Maintain a correction ledger with old behavior, new promise, compatibility
   effect, and direct proof. Require a human decision for otherwise-unapproved
   wire, persistence, replay, workload, or allocation changes.
9. Replace generic proof lists with a target-by-target proof matrix, including
   the exact crate command, old behavior at risk, and explicit `N/A` rationales.
10. Make difficult claims executable with counters or hooks, compile-fail
    capability tests, deterministic race barriers, and settlement ledgers.
11. Specify whether each regression guard is structural, lexical, or behavioral;
    define roots, exclusions, allowlists, fixtures, and fail-closed behavior.
12. Guard against intent-artifact names leaking into code, tests, comments,
    traces, or public documentation.
13. Make closure objective: every manifest row needs disposition, direct proof,
    blast-radius proof, reviewed SHA, and green workflow; all findings must be
    closed or launch blockers; a fresh reviewer must audit final `origin/main`.
14. Every valid adversarial finding must be fixed. A rejected finding requires
    recorded falsifying evidence and reviewer agreement.

The execution document must itself decide child terminal ownership under
pressure, keepalive authority after drain timeout, WebSocket broadcast semantics,
and the intentional low-level allowlist. Focused characterization tests must
protect restart ownership, keepalive admitted work, WebSocket delivery,
benchmark counts, and replay facts before migration.

## Execution Review 2

**Disposition: revise before implementation.**

The dependency graph, allowlist, correction policy, child-result pressure
semantics, WebSocket semantics, review disposition, and final certification are
now sufficiently settled. Seven blockers remain:

1. Correct extension and playground paths; enumerate all guide pages, three
   additional public docs, findings history, the perf deployment README, and
   explicitly preserve or remove the orphan dirty-root lockfile.
2. Add a framework-only terminal-observation prerequisite between #364 and the
   real-chat/job-queue migration.
3. Do not let the SQLite migration invent a framework waiter. Commit to an
   existing typed request or terminal report, or revise the plan separately.
4. Remove or define keepalive force-close semantics, and define event-only HTTP
   response status and completion boundaries.
5. Give every crate a named executable public-path smoke test and a focused
   characterization test, including features, fixtures, services, and
   environment. `cargo test --all-targets` alone is not direct proof.
6. Use structural parsing only for syntax. Scan comments as source text, exclude
   negative fixture data from production scans, and replace broad/numeric
   artifact-leakage rules with finite exact phrases.
7. Make `commits.txt` durable through a committed and pushed one-writer tracking
   branch with a defined resume procedure.

## Execution Review 3

**Disposition: revise before implementation.**

Review 2's substantive contract, proof, guard, and durability blockers are
resolved. Three finite-scope corrections remain:

1. The baseline already tracks the orphan
   `examples/systems/system_mini_saas_api/Cargo.lock`; assign its preservation or
   removal to C1 instead of treating it as an untracked dirty-root file.
2. Remove M1's wildcard ownership of any other supervised-child envelope. F1
   owns framework support and M1 owns exactly real chat and job queue.
3. `specimen_multi_turn_request_context` and `specimen_tcp_echo` have no local
   README. Require C1 to add the documented public runner before their standard
   smoke proof can be accepted.

## Execution Review 4

**Disposition: accepted for implementation.**

No remaining blocker. The execution document covers every current example
Cargo manifest and public documentation target, settles the framework contracts,
defines executable direct and blast-radius proof, gives every migration one
owner, and specifies dependency, review, CI, rebase, guard, and durable-resume
behavior without leaving a product decision to the implementer. One Grok
orchestration session can execute it as written.
