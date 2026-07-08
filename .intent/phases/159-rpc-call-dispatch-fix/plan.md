# RPC call dispatch fix

## The bug

`tina-rpc` request/reply dispatch is broken end-to-end.

- `Connection::route_request` reaches the router with `call(router, RegistryMsg::Route(..), timeout)`.
- The runtime routes `call()` traffic to `Isolate::handle_call`, not `handle`.
- `Registry` and `SingleService` implement only `handle` (returning `Effect::Reply` — the old implicit reply-slot model). Their `handle_call` is the trait default, which rejects with `CallRejectedReason::UnsupportedMessage`.
- Result: every rpc call resolves as `CallOutcome::Rejected(UnsupportedMessage)`. The registry (when reached at all) and the connection both map `Rejected` → wire `Error(Internal)`. Every request comes back `Internal`.

Why nobody caught it: every tina-rpc unit test drives these isolates via `.handle(...)` directly, never through a runtime `call()`. The only end-to-end exerciser is `examples/specimen_rpc`, which is out-of-workspace and CI never builds it. Its golden was even edited to document the broken output (`ok=0 full=3 other=1`).

## Reply model (traced)

- **Registry**: deferred. `Route` looks up the service, issues a downstream `call(service_addr, ServiceCall, timeout)`, and replies to the original caller only when that downstream call resolves (`ServiceResult` continuation). So it must capture the caller's `RequestContext<RouterReply>` and carry it through the downstream call, then `reply_to` on completion.
- **SingleService**: synchronous. `dispatch(ServiceCall) -> ServiceReply` is a pure decode → invoke → encode. So `handle_call` replies immediately via `call.reply(...)`.

## The fix

### SingleService (synchronous)
Add `handle_call` that replies directly:
```
fn handle_call(&mut self, msg, call) -> Effect<Self> {
    call.reply(self.handler.dispatch(msg))
}
```
Keep `handle` (harmless; nothing sends it, but a stray send stays a no-op-ish reply that the runtime drops). Implement `CallableIsolate`.

### Registry (deferred)
- `RegistryMsg::ServiceResult` changes from `ServiceResult(CallOutcome<ServiceReply>)` to `ServiceResult(RequestContext<RouterReply>, CallOutcome<ServiceReply>)`. Drop `Clone` on `RegistryMsg` (`RequestContext` is `!Clone`); nothing external relies on it.
- `handle_call`: `Route` → `route(request, call)`; unknown service replies `call.reply(UnknownService)`; known service does
  `call.defer(call(service_addr, ServiceCall, timeout)).reply(RegistryMsg::ServiceResult)`.
- `handle`: `ServiceResult(req, outcome)` → `finish` maps outcome (unchanged mapping table) and `reply_to(req, mapped)`. `Route` via plain send has no caller → no-op.
- Implement `CallableIsolate`.

Registration stays `register_with_capacity` (does not require `CallableIsolate`); wire protocol, outcome mapping, backpressure all unchanged.

## Tests
New `tina-rpc/tests/rpc_end_to_end.rs` driving the real runtime `call()` path via `ThreadedRuntime::call_blocking`:
- registry call to a registered method returns `Replied(RouterReply::Ok)` (fails on old code: `Rejected`).
- SingleService called directly returns `Replied(ServiceReply::Ok)`.
- unknown method → `UnknownMethod`; unknown service → `UnknownService`.
- full TCP roundtrip: bind → accept → spawn `Connection` → std client request → assert a `Reply` frame (`ok=1`). This is the coverage whose absence hid the bug.

Rewrite the registry/service unit tests that constructed the old `ServiceResult` shape or drove `Route` through `handle`.

## Specimen
Restore `examples/specimen_rpc` golden to `ok=1 full=3 other=0`; remove the "KNOWN BUG" notes from README and `src/tina_impl.rs` header. Verify with `cargo run --manifest-path examples/specimen_rpc/Cargo.toml -- both`.

## Hostile review
- **send vs call**: connection only ever `call`s the router; registry `call`s the service. Plain sends of `Route` have no caller → no-op is honest. Covered.
- **deferred reply**: registry uses the blessed `call_ctx.defer(call(..)).reply(..)` + `reply_to` pattern (same as mini_saas controller). `RequestContext` carried in the continuation message.
- **timeouts**: registry-side downstream `call` timeout still maps `Timeout → Internal` (server timeout is not a wire frame). Unchanged.
- **backpressure**: `Full`/`Closed`/`Rejected` outcome mapping preserved verbatim in `finish`.
- **tokio facade**: `tina-rpc-tokio` uses only the `Client` side, never `Registry`/`SingleService`. Unaffected; run its tests to confirm.
- **`Client`/`Connection`**: `Reply = ()`, driven by sends, never called. Correctly stay handle-only.
- **Clone drop on RegistryMsg**: audited — only `Route` is constructed externally; nothing clones the message.
