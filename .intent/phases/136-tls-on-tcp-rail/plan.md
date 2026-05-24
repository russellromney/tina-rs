# Phase 136: TLS On The TCP Rail

## Status

- Planned v2 (2026-05-24), after `Plan Review 1` (hostile) in `review.md`.
  v2 folds in: downstream `tina-http` impact, the one-pending-op pump rule,
  the four subtle-semantics that are the real work, comprehensive proof, the
  specimen rewrite, the DNS clarification, and a pointer to the removal phase.
- This is an **implementation** plan. The auditing is done (see `review.md`).
  Every step below lands code and proof; no step is investigation.
- Sequencing: lands before HTTP/2 TLS ALPN / mTLS (Phase 127), which inherit the
  substrate TLS sits on.

## Starting Facts (current truth in `tina-runtime/src/driver/tls.rs`)

- TLS uses `rustls::StreamOwned<Connection, std::net::TcpStream>` (`tls.rs:8-9`)
  — rustls in **blocking mode** over a **std blocking socket**.
- Server binds its own `std::net::TcpListener` (`tls.rs:993`); client uses
  `TcpStream::connect_timeout` (`tls.rs:933`). Neither touches Betelgeuse.
- Handshake/read/write/close run on a per-shard **worker thread** that drains
  serially (`tls.rs:4`). One op blocks all TLS on that shard.
- FINDINGS #16: Tina HTTPS server + client **cannot share a runtime** (the single
  worker deadlocks both sides of one handshake); `HttpsListener` carries a
  busy-wait `tls_accept_timeout` (`listener_tls.rs:44-72`) to yield between accept
  polls.
- Plain TCP already rides the per-shard Betelgeuse loop on the shard thread
  (`threaded.rs:220-221,1408-1409`). TLS is the only side door.
- `rustls` connection types are sans-I/O: `read_tls`, `process_new_packets`,
  `reader`, `writer`, `write_tls`, `is_handshaking`, `wants_read/write`,
  `alpn_protocol`. They never own a socket.
- `tls_connect(addr: SocketAddr, server_name, root_certs, timeout)`
  (`call/tls.rs:10-15`): addr is **pre-resolved**; SNI name is carried. DNS is a
  separate rail and **is not part of this phase**.
- The TCP/TLS rail rejects a second in-flight op on a stream with
  `CallError::ResourceBusy` (`driver/mod.rs:95,1223`).

## Purpose

Make TLS a layer over the runtime's own TCP rail instead of a separate
blocking-socket subsystem on a worker thread. Substrate swap under a stable
runtime API.

```text
my Tina service can connect/accept/read/write/close TLS, and that TLS rides the
same per-shard completion reactor as plain TCP — no extra thread, no second
socket stack, server and client can share one runtime
```

## Hard Constraints (the mistake this phase exists to prevent)

A reviewer must reject the implementation if any is violated, even with green
tests:

1. **No own OS socket.** No `std::net::TcpStream`/`TcpListener`,
   `connect_timeout`, or `bind` in the TLS path. TLS gets bytes only from the
   runtime's Betelgeuse-backed TCP stream/listener rail.
2. **No worker thread.** No `thread::spawn`, no `SyncSender`/`Receiver` command
   lane for TLS. `TlsWorkerLane` + `tls_worker_loop` are **deleted, not
   refactored**.
3. **rustls in sans-I/O mode.** Runtime owns a `ClientConnection`/
   `ServerConnection` keyed by `TlsStreamId`. `StreamOwned` is removed.
4. **TLS runs on the shard thread**, driven as the shard harvests Betelgeuse TCP
   completions. Only allowed off-shard escape is the optional handshake-crypto
   offload (TLS-5), evidence-gated.
5. **The runtime `tls_*` call signatures do not change** (`tls_connect`,
   `tls_connect_alpn`, `tls_bind`, `tls_bind_alpn`, `tls_accept`,
   `tls_accept_alpn`, `tls_read`, `tls_write`, `tls_close`, `tls_close_listener`).
   The **`tina-http` workaround surface DOES change** (see "Downstream Impact")
   and that change is part of this phase.
6. **The TLS layer owns the underlying `StreamId` exclusively** (the isolate
   never sees it) and **serializes its own internal TCP ops** — at most one
   underlying `tcp_read` *or* `tcp_write` in flight at a time. A single `tls_*`
   call may issue an interleaved *sequence* of TCP ops, never two concurrently.
   This is what keeps the one-pending-op rule from re-creating a deadlock.
7. **One pending TLS op per stream** stays true at the isolate boundary.
8. **Simulator TLS unchanged** — scripted semantic I/O, not cryptography. Live
   path only.

## Includes

### Core machine

- `TlsConnState` per `TlsStreamId`: the rustls `Connection`, a **bounded** inbound
  ciphertext buffer, a **bounded** outbound ciphertext buffer, the owned
  underlying `StreamId`, and the in-flight continuation (which TCP op this TLS op
  is waiting on, plus the TLS-op deadline).
- `pump_tls(stream_id)`: the sequential state machine. While progress is
  possible: if `wants_write`, drain `write_tls()` → one `tcp_write`; if input is
  needed, one `tcp_read` → `read_tls()` → `process_new_packets()`; surface
  plaintext via `reader()`; accept plaintext via `writer()`. Comments at the pump
  must state the one-op-at-a-time rule (Hard Constraint 6) at the point it
  matters, because that is the exact thing prior code got wrong.
- A per-turn ciphertext byte cap (mirror `tcp_read`'s `max_len`) so one pump turn
  cannot process unbounded records and starve the shard.

### The four subtle semantics (named state + per-item test)

- **Whole-op timeout.** The TLS-op deadline is computed once at the `tls_*` call;
  every internal TCP op gets the *remaining* budget. Not per-TCP-op.
- **`tls_write` backpressure.** Completes only when plaintext is encrypted **and**
  the ciphertext is fully written to the underlying TCP stream. Bounded out
  buffer surfaces a typed `Full`-class outcome rather than growing.
- **close_notify vs truncation.** Clean `close_notify` maps to clean close; abrupt
  TCP EOF mid-record maps to the existing error outcome. Distinct, matching today.
- **close-wins / tombstone.** Reproduce `cancelled_by_close` →
  `Failed(TargetClosed)`, cancellation, and late-completion tombstoning exactly
  (`tls.rs:82-85` today), now as on-shard state.

### Capability + docs

- `RuntimeCapabilities`: TLS family moves lane-backed/blocking →
  **completion-backed (rides the TCP rail)**. `tls_lane_capacity` removed or
  repurposed as a per-stream pending cap (decide at TLS-4).
- FINDINGS #16 → Closed. SYSTEM.md TLS paragraph rewritten. The review memo's
  Odin-divergence note updated to "TLS now on-substrate."

## Downstream Impact (must change in this phase)

- `tina-http/src/listener_tls.rs`: remove the busy-wait `tls_accept_timeout`
  (or convert it to a real accept timeout, default "wait"). `HttpsListenerConfig`
  changes accordingly. Accept is now completion-driven.
- `tina-http/src/keepalive.rs`: no signature change; re-point keepalive-over-TLS
  tests at a shared runtime.
- `examples/specimen_native_https/*`: rewrite so a **Tina HTTPS client and Tina
  HTTPS server run in one runtime** (today the client is stdlib rustls on a
  separate thread/process). Keep the `tokio+tokio-rustls` impl as an interop
  counterparty.
- `tina-http/tests/client_tls_smoke.rs`: fold into the same-runtime path; it only
  lived separately because of the deadlock.
- `examples/README.md:148`: rewrite the `specimen_native_https` description (drop
  "single-worker TLS lane" / "cannot share a runtime").

## Does Not Include

- HTTP/2 TLS ALPN end-to-end, mTLS, system root stores (DER-only stays) — Phase
  127. This phase only guarantees ALPN bytes still flow (the `alpn_protocol()`
  accessor already exists).
- Any simulator TLS change.
- Any plain-TCP rail change.
- A TLS connection pool.
- The HTTP/2 ">64KB response" flow-control deadlock — unrelated, not the TLS lane.

## Staged Delivery (each step lands code + proof)

- **TLS-1 — first client stream (real, not a spike).** Ship `TlsConnState` +
  `pump_tls` for the client: `tls_connect` → handshake → `tls_read`/`tls_write`
  → `tls_close` over a Betelgeuse client TCP stream. Proof: connect/echo/close
  with no worker thread spawned.
- **TLS-2 — full client path + cutover.** Move all client `tls_*` onto the rail;
  delete client std-socket + worker use; client TLS capability completion-backed.
- **TLS-3 — server path.** `tls_accept` over the Betelgeuse **listener** rail;
  wrap accepted streams with `ServerConnection`; same pump. Removes the accept
  busy-wait; server + client share one runtime.
- **TLS-4 — delete the lane.** Remove `TlsWorkerLane`, `tls_worker_loop`,
  `StreamOwned`, blocking `read_tls`/`write_tls`, std-socket imports. Update
  capabilities, SYSTEM.md, FINDINGS #16, the downstream HTTP/specimen surface.
- **TLS-5 — optional, evidence-gated.** Only if a measured workload shows
  handshake asymmetric crypto as a shard hot spot: offload **only** that step.
  Not default.

## Known Tradeoff (accepted on purpose)

Handshake asymmetric crypto (ECDHE/RSA) now runs on the shard thread. Steady-state
record crypto is cheap; a flood of *new* connections becomes shard CPU — the
ordinary "CPU-bound handler blocks its shard" property, now **visible and
boundable by accept rate** instead of hidden on a serial worker that deadlocks.
TLS-5 is the escape hatch. Recorded so it is a decision, not a surprise.

## How We Prove The New Behavior (direct proof — comprehensive)

1. **Same-runtime Tina-client ↔ Tina-server** in one process/runtime — the exact
   case FINDINGS #16 calls impossible. This passing **is** the closure of #16.
2. **Three-direction interop:** Tina↔Tina, Tina-client ↔ `tokio+tokio-rustls`
   server, stdlib-rustls client ↔ Tina-server (counterparties already exist in
   `specimen_native_https`).
3. **ALPN negotiates `h2`** via `tls_connect_alpn`/`tls_bind_alpn`.
4. **HTTP/1.1-over-TLS** and **keepalive-over-TLS connection reuse** in one
   runtime.
5. **Backpressure:** slow peer, huge record → bounded buffers surface typed
   `Full`/backpressure, no unbounded growth.
6. **close_notify vs truncation** map to distinct outcomes.
7. **Whole-op timeout:** a handshake slower than the deadline times out once,
   not per-TCP-op.
8. **Write-during-read:** force rustls `wants_write` mid-`tls_read`; assert no
   `ResourceBusy`, op completes (guards Hard Constraint 6).
9. **Cancellation / close-wins / tombstone parity** with the old lane.
10. **Bounded-overlap multi-connection** TLS (mirror the `tcp_echo` proof):
    listener stays up across sequential + overlapping clients, closes clean.
11. **Guard test:** `std::net::Tcp*` and `thread::spawn` absent from
    `driver/tls.rs` after TLS-4; a thread-count assertion shows no TLS worker.

## How We Prove We Did Not Break Old Intent (blast-radius proof)

- Native HTTPS server+client, HTTP/2-over-TLS ALPN, and keepalive-over-HTTPS
  tests pass on the new path.
- TLS cancellation/timeout/close-wins outcomes + trace facts match (re-run the
  TLS ResourceRail DST).
- The simulator TLS suite is unchanged and green (proves the live change did not
  leak into the sim contract).

## Pointer: removal of the broader old model

TLS is the first instance of a recurring anti-pattern — a blocking worker lane
that bypasses Betelgeuse (also storage, Phase 138; also unix-domain sockets,
which are sockets and could ride the substrate). Phase 136 cuts TLS over and
deletes the **TLS-specific** old code. A separate, yet-unplanned phase —
working name **Phase 140: Retire the bypass-Betelgeuse lane model** — should,
once 136 and 138 prove the pattern, remove the generic worker-lane scaffolding
and move/justify the remaining lanes: unix-domain sockets onto the substrate;
DNS resolver and process spawn kept as blocking lanes **with written
justification** (no Betelgeuse opcode for `getaddrinfo` / `fork`). That phase,
not this one, "removes the old model entirely."

## IDD Next Step

Plan v2 + Plan Review 1 are done. Next: `Implementation Review 1` after TLS-1
lands, then proceed through TLS-2..4. Begin coding only on go.
