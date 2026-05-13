# 076 — Server-Side HTTP/1.1 Keepalive

## Status

- Done: first form landed.
  `tina_http::HttpListener` / `HttpConnection` now serves multiple
  sequential requests on one TCP/TLS stream when
  `HttpLimits::keepalive_idle_timeout` is `Some(d)`. Per-request
  close intent is honored: HTTP/1.0, explicit `Connection: close`,
  parse error, service-call error, peer EOF, and idle timeout all
  close cleanly. Nine integration tests in
  `tina-http/tests/server_keepalive.rs` cover
  reuse, idle close, slow-loris on subsequent requests, body cap,
  mixed GET/POST flow, and the real keepalive pool driving the real
  listener over one accepted server stream.
- Closed: merged.
- Deferred: idle-timeout + max-requests-per-connection as separate
  knobs (one knob covers both for first form), 408 emission on
  slow-loris (waits for runtime `tcp_cancel_read` affordance),
  configurable per-method body caps.

## Goal

Make the native HTTP server keepalive-capable. Without server-side
keepalive, the keepalive client from #50 has nothing to talk to in
the Tina ecosystem — every test of the client against
`HttpListener` would force a reconnect after each response.

## Non-goals

- No HTTP pipelining (request 2 sent before request 1's response
  completes). The reset between iterations drops any read-ahead
  bytes; well-behaved HTTP/1.1 clients don't pipeline.
- No HTTP/2 or WebSocket upgrade.
- Chunked request bodies were not part of 076; they landed later in
  080 and should not be treated as a keepalive non-goal anymore.
- No max-requests-per-connection knob in first form. Server can
  always close on its own decision but the configuration surface
  doesn't grow until evidence demands it.
- No 408 emission on slow-loris between requests. The runtime
  rejects `tcp_close_stream` while a `tcp_read` is pending, so the
  current path stops the isolate and lets runtime cleanup close.
  Same precedent as the existing first-request slow-loris guard.

## Grug truth

```
serve request. write response.
if request say close, close. else loop back. read next request.
if no bytes for X, close.
if bad bytes, close.
if same connection do many requests, fast.
never claim connection alive when transport gone.
```

## Shape

```rust
let limits = HttpLimits {
    keepalive_idle_timeout: Some(Duration::from_secs(30)),
    ..HttpLimits::default()
};
let listener = HttpListener::<MyShard>::new(
    bind_addr, service, limits, service_call_timeout, conn_mailbox_capacity,
);
```

`None` keeps the legacy one-request-per-connection behaviour. The
default is `None` (opt-in) so this is a non-breaking change.

## Mechanism

`HttpConnection` already owned the per-request state machine. The
keepalive change:

1. Track `request_generation: u64`, bumped at the start of every
   iteration. The `HeaderDeadline` message carries this generation
   so a stale deadline from a prior iteration is recognised and
   dropped (same lesson as the keepalive client in PR #50).
2. `dispatch_to_service` reads `head.connection_close` from the
   parsed head to set `will_close`. `keepalive_idle_timeout: None`
   forces close regardless.
3. `handle_wrote` (response drained) and `handle_stream_chunk`
   (Eof) call `finish_response` instead of `begin_close`.
   `finish_response` either closes (when `will_close`) or resets
   per-request state and starts the next iteration with the idle
   timeout.
4. `reset_for_next_request` drops any read-ahead bytes (no
   pipelining), clears parsed head and streaming bookkeeping.

Service-error responses (`Full`/`Closed`/`Timeout`) and parse
errors continue to force `will_close = true` — the protocol state
is suspect after either, so closing is the safer move.

A streaming response that ends short of the declared
`Content-Length` (source replied `Eof` with bytes still owed) also
forces close — the wire framing now lies and the next request
cannot ride this connection.

## Tests

`tina-http/tests/server_keepalive.rs`:

- `three_sequential_requests_share_one_tcp_connection_when_keepalive_on`
- `http_1_0_client_closes_after_one_response_even_when_keepalive_on`
- `explicit_connection_close_header_closes_after_response`
- `idle_timeout_fires_and_listener_remains_healthy`
- `default_limits_serve_one_request_then_close`
- `slow_loris_on_second_request_fires_deadline_via_trace`
- `body_cap_still_enforced_on_subsequent_keepalive_request`
- `keepalive_serves_mixed_get_and_post_with_bodies`
- `keepalive_pool_reuses_native_listener_connection_across_requests`

The two slow-loris-style tests observe the deadline via the
runtime trace (`Sleep` `CallCompleted`) rather than peer-side FIN,
mirroring the precedent in
`server_pressure.rs::slowloris_partial_header_closes_within_header_read_timeout`.
The current runtime keeps `StreamId` ownership until shutdown, so
the kernel-side close happens at runtime drop — the testable
property is "the deadline fired and the listener is uncorrupted."

## Done means

- `HttpLimits::keepalive_idle_timeout` ships as opt-in.
- Sequential requests on one TCP connection succeed.
- Per-request close intent (HTTP/1.0, `Connection: close`) is
  honored.
- Idle timeout fires deterministically and the listener stays
  healthy after a slow-loris client.
- Existing one-request-per-connection behaviour is preserved when
  the knob is left `None`.
- All existing tests pass unchanged.
