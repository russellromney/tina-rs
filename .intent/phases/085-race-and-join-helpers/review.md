# 085 Plan Review

## Plan Review 1

### Finding 1 — Race report timing was ambiguous

The initial plan said first-success race cancels losers and returns a
report with cancellation outcomes, but it did not say whether the winner
is reported before or after loser cancellations settle. Returning early
while promising `cancelled: Vec<_>` would either lie or force a hidden
background report. The plan now pins first form: return after
loser-cancel outcomes are known. A future "reply winner immediately"
helper must admit its report is incomplete at reply time.

### Finding 2 — Outcome storage needs a concrete cap

`CallGroup` sketched `Vec<NamedOutcome<K, R>>` without saying the vector
is capped by group capacity. That is exactly how a helper meant to make
bounded waits easier could smuggle in an unbounded result collection. The
rules now say outcome storage is capped by group capacity.

### Finding 3 — Heterogeneous branch replies would balloon first form

Real `select!` can wait on unlike things. Tina should not start there.
Mixed reply types require a user enum or a much larger erasure story. The
plan now defers heterogeneous reply sets explicitly.

### Finding 4 — Required proof mixed first-success and join-all scope

The first proof list required both race and join behavior even though the
shape says two PRs max. The proof section now splits PR 1 from PR 2, so
the first implementation can stop after a useful first-success helper if
join-all gets fuzzy.

## Decision

Proceed with 085 only after 081, and preferably after 084 if child work
is the chosen specimen. The first implementation should prefer one boring
`CallGroup` helper plus a first-success race. Do not build a policy
framework or macro.

## Implementation Hostile Review

### Finding 1 — Token must exist before the call effect is built

The first draft shape returned the generation token from `insert`, but
`call_with_handle(...).reply(...)` needs to close over the token before
the helper can store the returned handle. That circular API would push
users toward key-only continuations and reopen ABA. Fixed by adding the
explicit `reserve_token` / `insert_reserved` path. `insert` remains for
tests or manual code that can route the token separately.

### Finding 2 — `Vec::with_capacity` is not itself a cap

The helper uses fixed-cap vectors, but hostile grug notes that ordinary
`push` can still grow. Branch outcomes are bounded by live branch count;
cancel outcomes now return a typed `StorageFull` error instead of
growing past capacity. Tests cover fill/full and fill/cancel/refill.

### Finding 3 — Report-before-cancel would lie

The runtime proof waits for `cancel_call` outcomes before the service
replies through `RequestContext`. The report is not considered complete
until loser cancel outcomes have been recorded. Late loser replies are
not delivered to the group; they remain runtime trace facts as
`CallerCancelled` rejections.

### Finding 4 — Join-all stayed out

Join-all was not small enough to add honestly in the same slice without
turning the helper into a policy surface. First-success shipped; join-all
remains deferred until it can use the same bounded report vocabulary
without hiding deadlines or partial truth.

### Finding 5 — Owner stop must be user-visible work

No `Drop` cancel was added. The owner-stop proof sends a stop message,
drains the group into named cancel requests, returns visible
`cancel_call` effects, waits for cancel outcomes, then stops with a
report.

## Follow-Up Hostile Fixes

### Finding 6 — Forged cancel acknowledgements completed reports

`record_cancel` originally counted cancel outcomes without proving they
matched the losers returned for cancellation. Fixed by retaining the
expected `(key, token)` rows and removing them one by one. Unknown or
duplicate cancel completions now return `UnexpectedCancel` and cannot
make the report complete.

### Finding 7 — Reserved tokens were reusable by hostile callers

`insert_reserved` originally trusted any copied `CallGroupToken`. Fixed
by tracking reserved tokens as bounded slot reservations. A token must
come from `reserve_token`, consumes group capacity while reserved, and
is removed on first successful `insert_reserved`. Reusing an old or
already-consumed token is a typed `UnknownReservedToken` error. A
validated reserved token is also released on typed insert failure, so
`DuplicateKey` does not leak capacity.

### Finding 8 — Specimen swallowed group errors

The cancellation-chain specimen no longer drops `record_reply` /
`record_cancel` errors. It records a group error and reports
`exit_clean = false`, making the typed helper failure visible in the
specimen result.
