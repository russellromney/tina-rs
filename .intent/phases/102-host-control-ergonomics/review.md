# Hostile Review - Phase 102

## Finding 1 [P2] Shutdown handle could hide service drain policy

The risky version of this phase would make `request_shutdown()` look like
"gracefully stop my application." That is not true unless the service has
already exposed its own Stop/Drain protocol. The plan now says the shutdown
handle controls runtime shutdown only; service drain remains explicit service
policy.

## Finding 2 [P2] Non-consuming shutdown can create double-join confusion

If two host threads wait for the same terminal report, silent second waiter
hangs would be awful, and single-claim `AlreadyJoined` would make host code
annoying. The plan now pins the better shape: cache terminal truth and let
multiple waiters receive equal `LocalSystemTerminalReport` values.

## Finding 3 [P2] `call_blocking_on` could infer the wrong shard

Inferring from the address is tempting, but this phase is about making host
control explicit. The plan keeps the shard argument and requires a
shard/address mismatch proof. A later convenience wrapper can infer if this
gets annoying.

## Finding 4 [P2] Timeout semantics need to match single-shard `call_blocking`

The helper must use the normal Tina call timeout and a host wait timeout, just
like the single-shard helper. It must not cancel accepted work or pretend that
timeout stopped the callee. The plan now calls out those outcomes and requires
a held-caller timeout test.

## Finding 5 [P3] Specimen migration could balloon

Real multi-shard placement can cause a specimen rewrite. Letting the worker
choose made the plan less executable. The plan now pins the migration:
`system_session_auth` proves real threaded multi-shard host calls, and
`system_metrics_shipper` proves the shutdown handle.

## Finding 6 [P3] Unknown-shard behavior must match local convention

Some current host APIs panic on unknown shard as programmer error. The plan
does not invent a new error vocabulary; it now pins `call_blocking_on` to panic
on unknown shard and shard/address mismatch, matching existing multi-shard host
API convention.

## Finding 7 [P2] Non-consuming shutdown needs an ownership refactor

A cloneable shutdown handle cannot produce the existing terminal report unless
it can claim the worker join handle and retained trace/topology truth. The plan
now says this explicitly: refactor threaded runtime internals so the first
waiter joins and caches terminal truth, and every later waiter or consuming
`shutdown_report(self)` returns that same cached report. This avoids a fake
polling-only shutdown handle that cannot return real terminal truth.

## Finding 8 [P3] Scope should not leak into the trait crate

This is host-control ergonomics, not a core service trait redesign. The plan now
pins API home to `tina-runtime`; `tina` trait-crate changes are out of scope.
