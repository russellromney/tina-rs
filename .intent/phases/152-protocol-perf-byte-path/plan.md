# Phase 152: Protocol Perf Rows And Byte Path Cost

## Status

- Follows Phase 151.
- Phase 151 removed the worker wakeup gap. HTTP/1 no longer spends about 1ms
  asleep between kernel readiness events.
- The next costs are now visible: protocol workload rows are uneven, the byte
  path still has compatibility copies in some protocols, and connection setup
  still costs real kernel round trips.

## Grug Truth

The worker is awake now. Stop blaming sleep.

Measure HTTP/2 and WebSocket like we measure HTTP/1. Remove the obvious byte
copies that still remain. Name the connection setup cost instead of hiding it
inside "HTTP is slow."

## Goal

Build the next native performance pass:

1. HTTP/2 and WebSocket equivalent workload rows.
2. Fewer-copy byte paths beyond the migrated HTTP/1 request/response paths.
3. Connection setup stage rows now that the old idle wait no longer hides them.

Done means:

- `examples/systems/perf_native` prints HTTP/2 and WebSocket rows with the same
  honesty as the HTTP/1 rows: same-work baseline where possible, p50/p90/p99,
  allocation counts, process rows, pressure/timeout truth, and leak-clean proof.
- Any protocol path still using a compatibility read/write helper is either
  migrated to the owned/reusable byte API or explicitly named as deferred with
  a reason.
- connection setup is measured separately from steady-state request work.
- no public production performance claim is made.

## Non-Goals

- no new scheduler;
- no new runtime park policy;
- no fake zero-copy claim if bytes are still copied;
- no HTTP/2 or WebSocket feature expansion unless needed to run the perf row;
- no benchmark-only code path that bypasses normal public service/client APIs;
- no broad Linux performance claim from one machine.

## Rock 1: HTTP/2 Equivalent Rows

Add native HTTP/2 perf rows in `examples/systems/perf_native`.

Use real Tina HTTP/2 service/client surfaces, not private shortcuts. The row
should be equivalent to an existing HTTP/1 row:

- small request/response;
- fixed body request/response if the public path supports it cleanly;
- keepalive / reused connection if HTTP/2 client/session reuse exists;
- same operation count and timeout budget as the comparison row.

If the closest external comparison is hyper/tonic and would make the row much
larger, keep the first row Tina-only plus a clear "no baseline yet" field. Do
not fake semantic equality.

Required output:

- `perf-compare` or `perf-native` line;
- p50/p90/p99;
- allocation count and allocated bytes if available;
- stage count / scheduler gap count where the harness can collect it;
- leak-clean and zero timeout proof.

## Rock 2: WebSocket Equivalent Rows

Add native WebSocket rows:

- connect/open;
- ping/pong or one small text round trip;
- steady-state N-message exchange over one session;
- slow/blocked peer overload row if it can be CI-sized and deterministic.

Use the public WebSocket session/client surfaces. The row must exercise the
normal app session path, not only frame encoding helpers.

Required truth:

- successful messages counted;
- close/drain is clean;
- pressure is typed if the row intentionally fills outbound capacity;
- no hidden unbounded queue in the test harness.

## Rock 3: Migrate Remaining Compatibility Byte Paths

Find protocol paths that still call old compatibility helpers or clone before
write after Phase 146.

Known places to check:

- HTTP/2 request/response body reads and writes;
- WebSocket server session writes;
- standalone WebSocket client reads/writes;
- gRPC-over-HTTP/2 body/trailer paths;
- codec/framing helpers used by these protocol paths.

For each path:

- prefer reusable read scratch or owned-buffer return;
- prefer borrowed/owned write that does not clone the bytes before the driver
  owns or writes them;
- preserve failure truth: if a driver error cannot return the buffer yet, name
  that as the remaining broader error-envelope gap instead of silently dropping
  ownership facts;
- keep DST trace hashes stable unless the public call kind really changes.

Do not migrate by adding a benchmark-only fast path. Normal app code gets the
win.

## Rock 4: Connection Setup Rows

Add rows that separate:

- connect/open;
- accept;
- first read/write;
- steady-state reused connection work.

The point is not to make connection setup vanish. It is real kernel work. The
point is to stop mixing it with steady-state service cost.

Required proof:

- HTTP/1 close vs HTTP/1 keepalive rows still print;
- new HTTP/2/WebSocket rows say whether they include setup or reuse;
- stage breakdown names setup-heavy stages in the same vocabulary as existing
  hotpath rows;
- no timing assertion is so tight that shared CI becomes flaky.

## Rock 5: Perf History And Docs

Update:

- `.intent/phases/152-protocol-perf-byte-path/perf_history.jsonl`;
- `examples/systems/perf_native/README.md`;
- `ROADMAP.md` done/remaining text;
- `CHANGELOG.md`.

Docs must say:

- what improved;
- what did not improve;
- what still allocates/copies;
- which rows are macOS-only or Linux/x86;
- no production performance claim yet.

## Proof

Run focused proof:

- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test perf -- --nocapture`
- `cargo test --release --manifest-path examples/systems/perf_native/Cargo.toml --test hotpath -- --nocapture`
- protocol tests touched by the byte-path migration:
  - `cargo test -p tina-http --all-targets`
  - relevant `tina-runtime` TCP/TLS/Unix call tests if runtime byte APIs change

Run regression proof:

- `cargo fmt --all --check`
- `cargo clippy -p tina-http -p tina-runtime --all-targets -- -D warnings`
- `make proof-fast`

If runtime call kinds or replay-visible events change, run the affected DST
tests and update fingerprints only with evidence in the phase notes.

Linux proof:

- collect at least one Linux/x86 release sample using the existing Fly or
  Ubuntu workflow and save it in the phase dir;
- if Linux cannot be run in the session, the PR must say so and leave Linux as
  an explicit pre-merge check, not an implied claim.

## Done

- HTTP/2 and WebSocket rows exist and are useful.
- Setup vs steady-state cost is visible.
- Obvious remaining compatibility byte copies in HTTP/2/WebSocket/gRPC paths
  are removed or explicitly deferred with a reason.
- Perf docs are honest.
- Existing HTTP/1 perf rows still pass.
- No deterministic simulator or proof-fast regression.
