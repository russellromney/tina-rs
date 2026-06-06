# Phase 152 Review

## Plan Review 1

Findings:

- [P2] The first plan could have become "add more rows" without changing any
  code. That would not be enough. The plan now requires byte-path migration for
  real protocol paths where compatibility helpers still copy or clone.
- [P2] "Equivalent workload" can become dishonest if the baseline is not
  semantically equivalent. The plan now allows Tina-only rows with a clear
  shape when a fair external baseline would be too large, and forbids fake
  semantic equality.
- [P2] Connection setup could be mistaken for a regression after Phase 151 made
  it visible. The plan now requires explicit setup vs steady-state rows and
  stage naming.
- [P2] WebSocket perf rows could accidentally test only frame helpers. The plan
  now requires the normal public session/app path.
- [P3] Linux proof could be implied by old Phase 151 evidence. The plan now
  requires at least one Linux/x86 sample for this phase, or a named pre-merge
  gap.

Decision:

- Plan is implementation-ready. It is not a planning phase. It builds rows,
  migrates byte paths, records setup cost, and updates docs with honest
  non-claims.
