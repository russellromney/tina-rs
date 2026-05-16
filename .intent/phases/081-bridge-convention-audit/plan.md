# 081 Bridge Convention Audit

## Status

- IDD phase.
- One PR.
- Runs after the first bridge family is real: tokio, tower, reqwest, sqlite,
  sqlx, rpc-tokio, and aws.
- Can run in parallel with 099/100/102 if it mostly edits bridge docs/crate
  docs. Coordinate if another branch owns the same bridge code.
- This is not a planning phase. Audit, fix mismatches, update docs, and ship
  only tiny helpers that are proved by repeated code.

## Grug Truth

Bridges are edges.

Edges must be boring:

- install means install;
- close means close;
- drain means drain;
- timeout means caller wait stopped, not always backend work stopped;
- late result truth is explicit;
- metrics are worker-terminal unless the crate can prove caller-terminal;
- tracing fields should not promise more than the bridge knows.

Do not build bridge framework fog.

If three bridges have the exact same small code shape, extract a tiny helper.
If they merely feel similar, write the convention down.

## Goal

Make bridge users see one coherent story across Tina bridge crates:

- how to install;
- what config is required;
- what is bounded;
- how to close/drain;
- what metrics mean;
- what tracing targets/fields mean;
- what late result/cancellation truth means;
- what supplied-client/runtime ownership means.

The result should help a cheap model pick the right bridge shape without
inventing `Arc<Mutex<_>>`, hidden retries, or fake cancellation.

## Non-Goals

- no new `tina-bridge-common` crate unless exact repeated code forces it;
- no broad API rename unless there is a real bug or stale public contract;
- no harmonizing away true differences between bridges;
- no new retry policy;
- no new cancellation semantics;
- no hiding backend/runtime ownership behind "easy" defaults;
- no large specimen migration.

## Rock 0: Inventory

Read these bridge crates and docs:

- `tina-tokio-bridge`;
- `tina-tower-bridge`;
- `tina-reqwest-bridge`;
- `tina-sqlite-bridge`;
- `tina-sqlx-bridge`;
- `tina-rpc-tokio`;
- `tina-aws-bridge`;
- `docs/tina-user-guide/18-bridge-crates.md`;
- bridge sections in `docs/tina-user-guide/11-ergonomics-checklist.md`;
- `examples/FINDINGS.md`.

Create a short inventory table in the phase status or docs:

| Crate | install | config | closer | metrics | tracing | late result | supplied client |
|---|---|---|---|---|---|---|---|

For each bridge, record:

- install return shape;
- close/drain API;
- config validation rules;
- in-flight/mailbox/pending caps;
- metrics counters and whether they are caller-terminal or worker-terminal;
- tracing target/event shape;
- timeout/cancel/late-result story;
- supplied-client or supplied-runtime ownership, if any.

## Rock 1: Install, Config, And Close Words

Fix stale or inconsistent docs where words overclaim.

Rules:

- `install(...)` should name what it owns;
- `install_with_*` should name what the caller owns;
- config validation should reject knobs that are ignored, or docs must say they
  are ignored on that path;
- `close` must say whether in-flight work finishes, is rejected, or is best
  effort;
- `drain` must say what counts as drained;
- send-only close messages must not look like call/ack APIs.

Do not force every bridge into the same API if the backend truth differs.

## Rock 2: Metrics, Pressure, And Late Results

Make metric docs honest.

Rules:

- worker-terminal counters are named as worker-terminal;
- caller-observed outcomes are named only when the bridge can observe them;
- `late_results` means exactly one thing per crate and the docs say it;
- if a backend task can continue after caller timeout but the bridge cannot
  observe completion, docs must say so;
- pressure reports must use installed/effective capacity, not caller-supplied
  config at report time.

Proof:

- at least one test or doc assertion for every fixed overclaim;
- if no code changes are needed, add a short status note saying which crates
  were checked and why docs-only is enough.

## Rock 3: Tracing Vocabulary

Check bridge tracing against docs.

Rules:

- event targets in docs match actual targets;
- terminal events carry the request metadata docs promise;
- no invented correlator is promised unless it exists;
- existing `tina-rpc-tokio` span shape is documented as existing shape unless
  this phase deliberately migrates it with tests.

If a field is only on admission events, docs must say that. If operators need
it on terminal events and the bridge has it, add it.

## Rock 4: Supplied Client / Runtime Ownership

Audit supplied-client and supplied-runtime paths.

Rules:

- caller-owned clients keep caller-owned settings;
- bridge-owned outer timeout/admission still stays visible;
- drop requirements are named, especially Tokio-context traps;
- no docs say "supplied pool/client" for first-form bridges that do not support
  it yet;
- no config path validates settings it promises to ignore.

## Rock 5: Tiny Helper Extraction, Only If Earned

Look for exact repeated code.

Allowed helpers:

- tiny capacity/pressure formatter;
- tiny installed-config snapshot helper;
- tiny tracing field helper;
- tiny closer/report adapter.

Not allowed:

- bridge framework crate;
- trait object over all bridges;
- generic retry/cancel/drain abstraction;
- helper that erases different backend truth.

Rule: three exact repeated shapes is evidence. Two is a note.

## Rock 6: Docs And Findings

Update:

- `docs/tina-user-guide/18-bridge-crates.md`;
- crate-level rustdocs for touched bridges;
- `docs/tina-user-guide/11-ergonomics-checklist.md` if copied bridge shape
  changes;
- `examples/FINDINGS.md` if findings are closed or moved.

Docs should include:

- one bridge convention table;
- one good copied install/close/metrics example;
- one warning box for timeout/cancel/late-result truth;
- one warning box for supplied-client ownership.

## Required Checks

Run at least:

```text
cargo fmt --all --check
cargo test -p tina-reqwest-bridge
cargo test -p tina-sqlite-bridge
cargo test -p tina-sqlx-bridge
cargo test -p tina-aws-bridge
cargo test -p tina-tokio-bridge
cargo test -p tina-tower-bridge
cargo test -p tina-rpc-tokio
cargo clippy -p tina-reqwest-bridge -p tina-sqlite-bridge -p tina-sqlx-bridge -p tina-aws-bridge --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

If a crate is excluded from the workspace or needs external services, record the
targeted command that is safe locally and let CI/ignored integration tests own
the rest.

## Success

- Bridge docs match code.
- Metrics and late-result words do not lie.
- Close/drain/cancel words are consistent enough to copy.
- Any helper shipped is tiny and proved by repetition.
- No bridge framework fog landed.
