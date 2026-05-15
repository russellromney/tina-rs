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
