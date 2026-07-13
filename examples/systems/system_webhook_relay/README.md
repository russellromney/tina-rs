# Webhook Relay

Tiny relay system that exercises the bridge classifier path. Each event is
sent through an outbound port; the relay reads the typed outcome, classifies
it (succeeded / transient / fatal), and replies with one of:

- `Delivered { backend_id }` — the SDK accepted the message;
- `Retry { reason }` — transient classifier (caller retries, idempotency is
  the caller's story);
- `DeadLetter { reason }` — fatal classifier (no retry without input change).

Hermetic by default: the bundled request-only `FakeOutbound` service returns
prepared typed outcomes. It is registered with `register_request_service` and
called through its request address, so the example carries no dummy event lane
or raw service envelope. No AWS account required.

The same relay can be wired to the SQS bridge through `SqsOutbound` and the
`map_sqs_outcome` helper. The mapping into `OutboundError` walks the typed
`SqsError` variants explicitly so the retry/dead-letter policy stays
visible.

## Run

```bash
cargo run --manifest-path examples/systems/system_webhook_relay/Cargo.toml
cargo test --manifest-path examples/systems/system_webhook_relay/Cargo.toml
```

## What is preserved

- bounded ingress: the relay isolate's mailbox; the outbound's `max_in_flight`;
- bounded host input: event count, fake-program length, and caller/bridge
  deadlines are validated before runtime startup or result allocation;
- typed visible outcomes: outer `Full`, `Closed`, `Timeout`, `Rejected`, and
  `ThreadedRuntimeError` remain distinct from worker-domain classifier results;
- caller-owned idempotency and retry budget: the relay does not retry, does
  not invent event ids, and does not infer idempotency from event content;
- two-layer truth: bridge-delivery (`CallOutcome::Full/Closed/Timeout`) and
  worker-outcome (`OutboundError`) are mapped distinctly into the classifier;
- missing SQS `message_id` is an `OutboundError::Internal`, never a successful
  empty backend id;
- `LocalSystem::run_to_shutdown_reported` owns bounded shutdown and retains a
  workload error alongside any shutdown failure.

## What is weakened

- replay determinism under `tina-sim` if a real bridge is wired in; bridge IO
  is not observed by the simulator;
- the `Retry` reply does not include a backoff suggestion; budget and timing
  remain caller-owned.

## Tina capability pulled

- multi-turn request/reply (relay defers its call's reply through the
  outbound port);
- event-only/request-only/split-service authoring and typed service handles;
- typed request continuations (`RequestCall` and `call_request`);
- bridge classifier (`BridgeOutcomeClass`, `BridgeRetryable`,
  `BridgeUnavailable`, `BridgeFatal`);
- typed-error mapping out of `tina-aws-bridge::SqsError`.

## Driver shape

The hermetic driver is intentionally sequential: one scripted outbound result
is consumed for each event in submit order. The example does not claim or
simulate concurrent callers. Applications that need a concurrent host wave
should use scoped caller threads and validate the caller cap before spawning
them.

## Suggested follow-up

- a tiny per-event journal isolate would make the `DeadLetter` path durable
  rather than just metric-visible;
- a backoff-budget isolate could ride alongside the relay to suggest delays
  for `Retry` outcomes without growing a hidden retry loop inside the relay.

## Authoring a bridge — copied path

The relay reuses an outbound bridge but does not author one. For the
copied "write a bridge" path — install, close, drain, metrics, pressure,
classifier, late-result truth — see
[`docs/tina-user-guide/30-bridge-author-kit.md`](../../../docs/tina-user-guide/30-bridge-author-kit.md).
The user-facing checklist starts from the bridge author's job, then maps
to `BridgeInstall`, `BridgeCloser`, `close_and_drain`, the metrics handle,
the pressure report, and the classifier vocabulary used by this relay.

## Verdict

- keep
