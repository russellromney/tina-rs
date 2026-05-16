# Hostile Review

## Finding 1 [P2] The phase can become docs-only

The goal is a copied service shape. If the specimen code does not change, the
phase probably failed.

Resolution: Rocks 1-3 require code-level migration/proof for multi-turn
requests, pressure, and shutdown.

## Finding 2 [P2] The service can accidentally teach old RequestContext ceremony

After Phase 095, ordinary multi-turn replies should use
`call_ctx.defer(work).reply(...)`. Carrying `RequestContext` manually
everywhere is now the lower-level form.

Resolution: Rock 1 requires `defer(...)` for copied ordinary paths and keeps
expanded `into_request_context()` only as explanatory material.

## Finding 3 [P2] Helper extraction can turn into a framework

Service skeleton work is tempting: route helpers, response helpers, shutdown
helpers, capacity helpers, and suddenly it is a web framework.

Resolution: Rock 4 keeps helpers tiny and earned. One-system-only glue stays
local.

## Finding 4 [P3] Pressure proof can be print-only

The old pressure command could look useful while not asserting the pressure
fact.

Resolution: Rock 2 requires a pressure command/test that asserts a visible
`Full`, `Closed`, or `Timeout`.

## Finding 5 [P3] Shutdown can overclaim clean drain

Real shutdown has in-flight work, closed pools, and rejected new work.

Resolution: Rock 3 requires shutdown while one request is in flight plus a
post-shutdown rejection/closed proof.

## Finding 6 [P2] Recent service helpers can make the skeleton stale on day one

Phase 101 shipped recurring tick, drain, local permit, register/bootstrap, and
Full-handling helpers. A refreshed skeleton that keeps hand-rolled versions of
those patterns would teach old Tina.

Resolution: status and grug truth now require reading Phase 101 and using the
helpers where they reduce ceremony without hiding message/effect truth.

## Finding 7 [P2] The realtime-rooms failure can leak into the copied service

`system_realtime_rooms` showed a sharp request/event bug: work started from
`handle_call` can accidentally route later runtime completions back through
`handle_call` as public requests. A production skeleton must not copy that.

Resolution: Rock 1 now names the footgun and says request-started work must
land as internal events unless another public request endpoint is intended.

## Finding 8 [P3] Host shutdown ceremony may churn after Phase 102

If Phase 102 lands first, `Arc::try_unwrap(runtime)` or one-off shutdown driver
code becomes stale immediately.

Resolution: status and Rock 3 now say to use `ThreadedShutdownHandle` when it
exists, and to leave a migration note if this phase runs first.
