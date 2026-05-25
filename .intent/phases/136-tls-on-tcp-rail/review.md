# Phase 136 Review (append-only)

## Plan Review 1 — hostile (2026-05-24)

Verdict: direction is right, but the plan as written would either (a) let the
mistake recur in a new shape, or (b) ship with the central claim unproven. Six
holes. All are folded into plan.md v2.

### Hole 1 — "API unchanged" is false downstream, and that hid real work

Hard Constraint 5 ("the isolate-facing API does not change") is true for the
runtime `tls_*` calls but wrong for the layer that exists *because of* the lane.
Code that must change:

- `tina-http/src/listener_tls.rs:44-72,222,249,267` — `tls_accept_timeout`
  (default 250ms) is a **busy-wait yield knob**, there only because the single
  TLS worker poll-loops on accept. On the new completion-driven accept it becomes
  either a real accept timeout or is removed. Either way `HttpsListenerConfig`
  changes. This is a public-ish config change the v1 plan denied existed.
- `tina-http/src/keepalive.rs:104,558-567` — the HTTPS client pool calls
  `tls_connect/tls_read/tls_write/tls_close`. Signatures stay, so no code change,
  but the keepalive-over-TLS tests that currently dodge the deadlock must be
  re-pointed at a shared runtime.
- `examples/specimen_native_https/*` and `tina-http/tests/client_tls_smoke.rs`
  exist *split* (server in Tina, client in stdlib rustls / separate file) only
  because client+server can't share a runtime. `examples/README.md:148` states
  this in prose. All three must change.

Fix: rephrase the constraint — *runtime `tls_*` call signatures do not change;
the `tina-http` workaround surface (accept-timeout busy-wait, split specimen,
separate-process tests) does change, and that change is part of the deliverable.*

### Hole 2 — the pump fights the one-pending-op rule (a new deadlock waiting to happen)

The TCP/TLS rail rejects a second in-flight op on a stream with
`CallError::ResourceBusy` (`driver/mod.rs:95,1223`). The new pump must, within a
single `tls_read`, sometimes `tcp_write` (rustls `wants_write` during a read:
alerts, post-handshake messages, key updates) *and* `tcp_read`. If the pump ever
issues a second underlying op before the first completes, it hits `ResourceBusy`
and stalls — the exact class of bug we're removing, reincarnated.

Fix (now a Hard Constraint): the TLS layer **owns the underlying `StreamId`
exclusively** (the isolate never sees it), and the pump **serializes its own
internal TCP ops** — at most one underlying `tcp_read` *or* `tcp_write` in flight
at a time, driven as a sequential state machine. A single `tls_read` may issue an
*interleaved sequence* of `tcp_read`/`tcp_write`, but never two at once. This
must be stated in code and pinned by a test that forces a write-during-read.

### Hole 3 — the subtle correctness core is underweighted

The hard part of TLS isn't the happy path; it's four semantics the v1 proof list
doesn't pin:

1. **Timeout spans the whole TLS op, not each TCP op.** A `tls_read` with a 1s
   timeout that issues 4 internal `tcp_read`s must not give each one a fresh 1s.
   The deadline is computed once at the `tls_*` call and threaded as remaining
   budget into every internal TCP op.
2. **`tls_write` backpressure.** It must complete only when the plaintext is
   encrypted *and* the ciphertext is fully written to the underlying TCP stream —
   not when rustls buffers it — or backpressure vanishes and buffers grow
   unbounded.
3. **close_notify vs truncation.** rustls distinguishes a clean `close_notify`
   from an abrupt TCP EOF mid-record. The `tls_*` outcomes must map these
   distinctly (clean close vs `CallError`), matching today's behavior.
4. **close-wins / tombstone.** The current lane has `cancelled_by_close` +
   synthetic `Failed(TargetClosed)` (`tls.rs:82-85`). The on-shard model must
   reproduce close-wins, cancellation, and late-completion tombstoning exactly.

These move into Includes as named state, and each gets a direct test.

### Hole 4 — proof is not comprehensive enough for the claim

The headline claim ("TLS rides the substrate; server+client share a runtime") is
proved by exactly one missing test. Comprehensive proof list is now in plan.md
v2 and includes: the **same-runtime Tina-client ↔ Tina-server** test (the #16
killer), **three-direction interop** (Tina↔Tina, Tina-client↔non-Tina-server,
non-Tina-client↔Tina-server — the specimen already ships a `tokio+tokio-rustls`
counterparty and a stdlib-rustls client to reuse), **ALPN negotiates `h2`**,
**HTTP/1.1-over-TLS and keepalive-over-TLS in one runtime**, **bounded
buffer / slow-peer / huge-record** backpressure, **close_notify vs truncation**,
**timeout-spans-op**, **write-during-read**, **bounded-overlap multi-connection**
(mirror the `tcp_echo` proof), and the **no-std-socket / no-thread grep guard +
thread-count assertion**.

### Hole 5 — DNS is independent; drop the implied DNS work

`tls_connect` takes a `SocketAddr` + SNI `server_name` (`call/tls.rs:10-15`).
DNS resolution happens before TLS via the separate `dns_*` rail. TLS needs the
SNI string for cert validation, which it already has. **No DNS change is in this
phase.** (DNS as a lane is a separate question for the removal phase below — but
DNS genuinely must stay a blocking lane: `getaddrinfo` has no Betelgeuse opcode.)

### Hole 6 — "spike" reads like audit; this phase is implementation only

Per the directive: the auditing happened in this review. TLS-1 is reframed from
"prove it's possible" to "the first shippable implementation increment (one
client stream end-to-end)." No step in plan.md v2 is investigation; every step
lands code + proof.

### Added: pointer to the removal phase

TLS is the first instance of a recurring anti-pattern: a blocking worker lane
that bypasses Betelgeuse (also storage — Phase 138; also unix-domain sockets,
which *are* sockets and could ride the substrate). Phase 136 cuts TLS over and
deletes the TLS-specific old code, but the **generic lane-bypass model** —
the shared worker-lane scaffolding and the lanes that don't truly need a blocking
thread — should be retired in one deliberate sweep once TLS (136) and storage
(138) prove the pattern. plan.md v2 points at that unplanned phase
(working name: **140 — Retire the bypass-Betelgeuse lane model**). DNS and
process spawn are expected survivors with written justification.

### Not in scope (do not conflate)

The HTTP/2 ">64KB response" deadlock (`http2/server.rs:1245`,
`http2_client_adversarial.rs:801`) is an HTTP/2 flow-control test limitation, not
the TLS lane. It is out of scope here and must not be folded into the #16 story.

## Plan Review 2 — re-verification against `origin/main` a6cbaa9 (2026-05-24)

Plan Review 1 (and plan v2) were authored against a stale branch. Re-verified
every Starting Fact on main. One premise was wrong:

- **The single-serial-worker / deadlock / "can't share a runtime" framing is
  stale.** Main reworked TLS to **one spawned OS thread per in-flight TLS op**
  (`tls.rs:601` `thread::Builder...spawn("tina-tls-{call_id}")`, bounded by
  `tls_lane_capacity` → `CallError::TlsFull`, reaped at `tls.rs:798`). FINDINGS
  #16 is already **"resolved first form"** by this rework; the quiet-stream test
  `local_system_tls_quiet_stream_does_not_block_second_connection` proves
  server+client share a runtime *today*.
- **Consequence for the plan:** the motivation is no longer "fix the deadlock"
  and the headline proof is no longer a "#16 killer." The real TPC defect is now
  **per-operation OS-thread spawn/reap churn on the hot path** — strictly worse
  than a serial worker. Plan v3 reframes Starting Facts, motivation, Staged
  Delivery (TLS-4 deletes the per-op spawn + `reap_finished_workers`), and makes
  the **headline proof "zero TLS threads spawned"**.
- **Facts that still hold (verified):** `StreamOwned` over std `TcpStream`/
  `TcpListener` (`tls.rs:8-9`, connect `:934`, bind `:994`); `tls_connect` takes a
  resolved `SocketAddr` + SNI (`call/tls.rs:10-15`, DNS independent); one-pending-op
  → `ResourceBusy` (`driver/mod.rs:95,1222`); plain TCP on the Betelgeuse loop on
  the shard thread (`threaded.rs:221,1423`); rustls sans-I/O API available.
- **Line numbers** drifted from the stale-branch citations and are corrected in
  plan v3.

Everything else in Plan Review 1 stands.

## Plan Review 3 — hostile, IDD completeness/intent/testing pass (2026-05-24)

v3 is strong on architecture. Four gaps against the IDD bar (intent =
"what will not change," and "how could this break while tests pass?").

### Finding 1 (blocking) — no proof for the scariest TLS failure: a handshake that succeeds when it must fail

The proof list has interop + ALPN but **no adversarial negative test**. Checked
main: there is essentially none today either (`local_system.rs:2698` only checks a
generic `failed:TlsHandshake`/`Timeout`). A sans-I/O rewrite can silently weaken
verification (forgotten verifier, `process_new_packets` error treated as success).
That is the #1 thing this rewrite can break, and nothing guards it.
**Required plan change:** add to "what will not change" a **security-posture
invariant** (cert verification, SNI/name check, DER-only root policy, no
downgrade) with adversarial tests: wrong-CA / expired cert → handshake rejected;
wrong SNI → rejected; truncated handshake → rejected. These are `direct proof` of
the invariant, not blast radius.

### Finding 2 — ALPN-over-TLS is already live; the sequencing/Does-Not-Include framing understates blast radius

The plan says it "lands before HTTP/2 TLS ALPN (Phase 127), which inherit the
substrate" and lists HTTP/2 TLS ALPN under Does Not Include. But ALPN over the TLS
rail **already exists on main**: `tina-http/src/lib.rs:88-90` ("`Http2Target::Tls`
dials the TLS rail with `h2` ALPN", `Http2ClientOutcome::TlsAlpnMismatch`),
`AlpnProtocols::h2()`. So ALPN is a **live consumer this phase must not break**,
not a future phase that inherits. **Required plan change:** move "HTTP/2-over-TLS
ALPN negotiation + `TlsAlpnMismatch` still fire" into blast-radius proof; reword
the sequencing note (127 is not a prerequisite for ALPN existing).

### Finding 3 — `tls_lane_capacity` removal is an unowned public-config change

"removed or repurposed (decide at TLS-4)" defers a **public config** decision
(`HttpsListenerConfig`, LocalSystem config) past implementation. IDD says escalate
public-API changes. **Required plan change:** decide now — keep the name as the
per-stream pending cap (least churn) or deprecate with a migration note — and add
a blast-radius check that existing configs setting `tls_lane_capacity` still
compile/behave.

### Finding 4 — the no-thread guard is grep-scoped too narrowly, and the close-wins parity cites stale lines

- Guard 11 greps `driver/tls.rs` for `thread::spawn`. A pump helper in another
  module (e.g. a new `driver/tls_pump.rs` or `driver/mod.rs`) would evade it.
  **Required:** scope the guard to the whole TLS path, and pair it with a runtime
  assertion that no `tina-tls-*` thread name is ever created (the per-op threads
  are named, so this is a precise live check).
- The close-wins/tombstone parity cites `tls.rs:82-85`, a **stale-branch** line
  range for the old single-worker `cancelled_by_close`. Main's per-op-thread model
  has different cancellation code (per-op `cancelled` AtomicBool + reaper).
  **Required:** re-anchor the parity target to main's current per-op cancellation
  semantics and fix the citation.

### Finding 5 — what is the oracle for TLS?

The plan says "simulator TLS unchanged, live path only." Confirm in Starting Facts
whether TLS exists on the explicit-step runtime at all, or whether its only
determinism story is sim-scripted TLS. If sim-scripted is the only oracle, say so
plainly so the DST claim is not read as more than it is.

### Keep

Hard Constraints 1–8, the four-subtle-semantics section, the per-op-thread-churn
motivation, and the staged cutover are all good and stay.

## Plan Review 4 — second reviewer (2026-05-25)

Verdict: good plan, but one boundedness phrase was wrong enough to send an
implementer sideways.

### Finding 1 — `tls_lane_capacity` must remain shard-total, not per-stream

The plan said `tls_lane_capacity` becomes a "per-stream pending cap." That would
weaken the old public budget: capacity 64 used to mean at most 64 admitted TLS
ops on the shard; "64 per stream" could admit unbounded-ish total work as stream
count grows. Fixed in plan v5: `tls_lane_capacity` is the **per-shard total
pending TLS-op cap**. One pending op per stream still holds separately.

### Finding 2 — explicit-step TLS must not blur simulator truth

The plan's "bonus" language could be read as requiring cryptographic TLS in the
simulator. Fixed in plan v5: `tina-sim` scripted TLS stays unchanged. The live
threaded path is required; explicit-step runtime TLS may improve only if it
naturally rides an existing explicit TCP rail.
