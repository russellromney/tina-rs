# Phase 124: HTTP/2 And Multi-Shard Fairness Hardening

## Status

- Ready.
- One PR.
- Can run beside Phase 114.
- Coordinate with Phase 123 if both touch `tina-http/src/http2.rs` or
  `tina-runtime/src/threaded_multi_shard.rs`. If conflict gets ugly, land
  Phase 123 first, then rebase this phase.

## Goal

Fix the second-pass adversarial findings A8-A12 from
`docs/adversarial-review.md`.

This is not another review phase. The review is done. Build the fixes.

Grug truth:

- HTTP/2 `content-length` must tell the truth;
- duplicate pseudo-headers are malformed, not last-value-wins;
- core HTTP/2 frames are not extension frames;
- remote shard traffic must not starve local control commands;
- tests prove the bad user-visible cases.

## Finding Coverage Map

- A8: HTTP/2 request `content-length` is not enforced.
- A9: HTTP/2 known-length streaming response `content-length` is not enforced.
- A10: HTTP/2 duplicate pseudo-headers overwrite instead of reject.
- A11: HTTP/2 `CONTINUATION` / standalone `PRIORITY` frame validation is weak.
- A12: multi-shard remote inbound flood can starve local commands/shutdown.

Every finding above must be fixed with a named test, or marked already-fixed /
false in `docs/adversarial-review.md` with the proof and test name.

## Rock 1: HTTP/2 Request Content-Length Truth

Fix A8.

Implement one shared request-body length rule for HTTP/2:

- parse every `content-length` header during header validation;
- reject invalid decimal values;
- reject conflicting duplicate values;
- reject duplicate equal values unless current HTTP/2 policy explicitly allows
  them. Preferred first form: one `content-length` header only;
- store expected request length on the stream;
- count received DATA bytes against the expected length;
- reject DATA that exceeds declared length before dispatching extra bytes to
  service code;
- reject END_STREAM before the declared length is fully received;
- apply the same rule to ordinary buffered requests, streaming request bodies,
  and gRPC request paths.

Required tests:

- buffered request with `content-length: 0` and DATA rejects before handler sees
  a non-empty body;
- buffered request with declared length shorter than DATA rejects;
- buffered request with declared length longer than DATA rejects on END_STREAM;
- streaming request with declared length shorter than DATA rejects before the
  service receives extra body bytes;
- streaming request with declared length longer than DATA rejects on EOF;
- gRPC request path obeys the same declared-length rule;
- invalid content-length value rejects;
- conflicting duplicate content-length rejects;
- equal duplicate content-length rejects, unless the implementation explicitly
  keeps equal duplicates legal and documents/tests that choice.

## Rock 2: HTTP/2 Known-Length Response Truth

Fix A9.

When Tina emits an HTTP/2 streaming response with a declared length:

- store `remaining_content_length` per stream;
- decrement it only when DATA is accepted for outbound send;
- never send more DATA bytes than declared;
- if the source emits too many bytes, reset/cancel the response visibly;
- if the source ends early, reset/cancel the response visibly;
- do not emit END_STREAM for a known-length response until remaining length is
  zero;
- metrics/trace should name short-source and overrun failures if a response
  lifecycle event already exists. Do not invent a fake success.

Required tests:

- `stream_known_length(N)` over HTTP/2 sends exactly N bytes then END_STREAM;
- source sends N in many chunks and succeeds;
- source EOF before N resets/cancels visibly;
- source sends N + 1 resets/cancels before extra byte is delivered as success;
- flow-control split DATA frames still decrement remaining length correctly;
- zero-length known stream succeeds with END_STREAM and no DATA;
- client/specimen path sees typed failure for short/overlong source.

## Rock 3: HTTP/2 Header And Frame Strictness

Fix A10 and A11.

Pseudo-headers:

- reject repeated `:method`, `:path`, `:scheme`, `:authority`, and `:status`;
- reject duplicates before assignment, so last-value-wins cannot happen;
- keep current required pseudo-header validation, plus the duplicate check.

Core frames:

- define `FRAME_CONTINUATION`;
- until full continuation support exists, reject standalone or unexpected
  CONTINUATION with connection-level protocol error;
- if full continuation support is implemented now, it must preserve HPACK block
  order and enforce END_HEADERS. No half-support;
- validate standalone `PRIORITY`:
  - stream id must be nonzero;
  - payload length must be exactly 5;
  - malformed frame is protocol error;
- do not ignore malformed core frames as unknown extensions.

Required tests:

- duplicate `:method`, `:path`, `:scheme`, `:authority`, and `:status` each
  reject;
- duplicate pseudo-header after regular header rejects;
- standalone CONTINUATION rejects;
- CONTINUATION after completed header block rejects;
- PRIORITY with stream id 0 rejects;
- PRIORITY with payload length not 5 rejects;
- valid PRIORITY frame either updates priority state or is accepted as a valid
  no-op, but only after shape validation;
- unknown extension frame still follows extension-frame policy, proving core
  frame validation did not make all unknowns fatal.

## Rock 4: Multi-Shard Local Command Fairness

Fix A12.

Current risk: a multi-shard worker reads local commands only when remote inbound
drain delivered zero envelopes. A sustained remote flood can starve host
`Run`, `call_blocking` setup, and `Shutdown`.

Implement simple fairness:

- after each bounded remote-drain pass, poll and run at least one local command
  if one is waiting;
- `Shutdown` has priority over ordinary remote envelopes once observed;
- `Run`, call setup, observer registration, and shutdown commands cannot wait
  behind an infinite remote flood;
- keep remote drain budget nonzero; fairness is not "stop draining remote";
- keep behavior identical in live and simulator where both have the same shape;
- add trace/metric if remote drain yields to local command under pressure if a
  suitable runtime pressure event already exists. Do not add noisy tracing just
  for style.

Required tests:

- sustained remote inbound flood does not starve a local host `Run` command;
- sustained remote inbound flood does not starve `call_blocking` setup;
- shutdown under sustained remote inbound flood begins within a bounded number
  of worker turns;
- normal remote throughput still progresses after fairness change;
- remote flood plus full target mailbox still returns typed terminal outcomes
  once Phase 123 terminal-reply work is present, or test documents the current
  dependency if Phase 124 lands first;
- simulator/live tests use the same fairness expectation where possible.

## Rock 5: Docs And Review Artifact

Update docs after fixes:

- `docs/adversarial-review.md`: mark A8-A12 fixed with test names, or mark
  false/already-fixed with proof and test names.
- `CHANGELOG.md`: record user-visible HTTP/2 strictness and multi-shard
  fairness hardening.
- `ROADMAP.md`: remove A8-A12 follow-up notes if present.
- If Phase 123 and Phase 124 both touch the same wording, keep Phase 123 as
  broad first-pass hardening and Phase 124 as the second-pass A8-A12 bundle.

## Tests To Run

Minimum targeted checks:

```sh
cargo fmt --all --check
cargo test -p tina-http http2 --tests -- --nocapture
cargo test -p tina-http grpc --tests -- --nocapture
cargo test -p tina-runtime --test multishard_dispatcher -- --nocapture
cargo test -p tina-sim multishard --tests -- --nocapture
cargo clippy -p tina-http --tests -- -D warnings
cargo clippy -p tina-runtime --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

If test names differ, run the closest crate-level targeted tests and record the
actual commands in the PR.

## Done Means

- A8-A12 each have a fix/proof/test.
- HTTP/2 request body length cannot lie to service code.
- HTTP/2 known-length streaming responses cannot lie to clients.
- duplicate pseudo-headers and malformed core frames reject visibly.
- local multi-shard control commands run under remote pressure.
- The review doc, changelog, and roadmap agree with shipped behavior.
