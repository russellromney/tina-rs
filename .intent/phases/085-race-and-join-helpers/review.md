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
