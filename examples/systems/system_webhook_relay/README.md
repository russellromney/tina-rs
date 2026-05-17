# Webhook Relay

Tiny relay system that exercises the bridge classifier path. Each event is
sent through an outbound port; the relay reads the typed outcome, classifies
it (succeeded / transient / fatal), and replies with one of:

- `Delivered { backend_id }` — the SDK accepted the message;
- `Retry { reason }` — transient classifier (caller retries, idempotency is
  the caller's story);
- `DeadLetter { reason }` — fatal classifier (no retry without input change).

Hermetic by default: the bundled `FakeOutbound` isolate returns prepared
`OutboundOutcome` values. No AWS account required.

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
- typed visible outcomes: every reply is one of three classifier-shaped
  variants;
- caller-owned idempotency and retry budget: the relay does not retry, does
  not invent event ids, and does not infer idempotency from event content;
- two-layer truth: bridge-delivery (`CallOutcome::Full/Closed/Timeout`) and
  worker-outcome (`OutboundError`) are mapped distinctly into the classifier.

## What is weakened

- replay determinism under `tina-sim` if a real bridge is wired in; bridge IO
  is not observed by the simulator;
- the `Retry` reply does not include a backoff suggestion; budget and timing
  remain caller-owned.

## Tina capability pulled

- multi-turn request/reply (relay defers its call's reply through the
  outbound port);
- bridge classifier (`BridgeOutcomeClass`, `TransientReason`, `FatalReason`);
- typed-error mapping out of `tina-aws-bridge::SqsError`.

## Suggested follow-up

- a tiny per-event journal isolate would make the `DeadLetter` path durable
  rather than just metric-visible;
- a backoff-budget isolate could ride alongside the relay to suggest delays
  for `Retry` outcomes without growing a hidden retry loop inside the relay.

## Verdict

- keep
