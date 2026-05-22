# Specimen Webhook Publisher

Tina-vs-Tokio: a small "counter that POSTs each new value to an
external webhook." The Tokio side runs `reqwest::Client::post` in a
loop. The Tina side runs through `tina-reqwest-bridge` and
deliberately uses **three different call-site shapes** in the same
file so they can be read side-by-side:

1. **`send_request(...).then(...)`** — the polished helper. The
   recommended default.
2. **literal `call(addr, ReqwestMsg::Send(req), timeout)`** — the raw
   layered form. Same outcome, more boilerplate. Kept in a real call
   site so the underlying contract stays exercised in code, not only
   in unit tests.
3. **`send_request(...)` with `flatten_outcome(...)` inside the
   reply translator** — the opt-in flat-error edge.

Both sides target the same fake webhook server. The driver runs
3 increments; the webhook should record bodies `["1", "2", "3"]` in
order.

## Run

```bash
cargo run --manifest-path examples/specimen_webhook_publisher/Cargo.toml -- compare
cargo run --manifest-path examples/specimen_webhook_publisher/Cargo.toml -- tokio
cargo run --manifest-path examples/specimen_webhook_publisher/Cargo.toml -- tina
```

Both sides report:

```text
side=tokio bodies=["1", "2", "3"]
side=tina  bodies=["1", "2", "3"]
```

## Read

- [`src/tokio_impl.rs`](src/tokio_impl.rs) — three sequential
  `client.post(...).await` calls inside a `block_on`. About 30
  lines.
- [`src/tina_impl.rs`](src/tina_impl.rs) — a `Driver` isolate that
  walks a counter from 1 to 3, POSTing each new value through the
  reqwest bridge. Each increment uses a different call-site shape;
  the file is the comparison.

## What this comparison taught us

### Tokio side

- Trivial. `client.post(url).body(value).send().await` in a
  three-iteration loop. The `Arc<Mutex<u64>>` for the counter is
  there because the example might one day fan out to concurrent
  posters; the example doesn't actually need it. Even with the
  mutex, it's about 25 lines.
- No back-pressure surface. If the webhook is slow or the request
  pile-up grows, nothing here notices until something elsewhere
  runs out of file descriptors or memory.

### Tina side

- `tina-reqwest-bridge::install(...)` returns the bridge handle in
  one call. The `Driver` isolate's state is `(http: ReqwestAddress,
  url, counter, timeout, done)` — five fields, all named. The
  isolate's message enum is four variants: `Run`, three
  per-shape `Posted*` continuations.
- The three call shapes work, all three reach the same webhook.
  Comparing them in one file is the point of the specimen.

### How the three shapes feel

**Shape 1 — `send_request(...).then(...)`:**

```rust
send_request(self.http, request, self.timeout)
    .then(DriverMsg::PostedViaSendRequest)
```

This is what users should reach for. Three positional args plus a
function-pointer translator. Reads exactly like
`tcp_read(...).then(...)` and the other native runtime calls. No
mystery; no `ReqwestMsg::Send(...)` wrapping at the call site.

**Shape 2 — raw `call(addr, ReqwestMsg::Send(req), timeout)`:**

```rust
call(self.http, ReqwestMsg::Send(request), self.timeout)
    .then(DriverMsg::PostedViaRawCall)
```

Functionally identical. One extra layer (`ReqwestMsg::Send(...)`)
to read. Not bad — it's spelled out, not magical — but if you write
five of these in a row, the wrapping starts to feel like
boilerplate. Worth keeping in *one* real call site so the
underlying bridge contract stays exercised in code, not just in
unit tests.

**Shape 3 — `flatten_outcome(...)` inside the reply translator:**

```rust
send_request(self.http, request, self.timeout)
    .then(|outcome| DriverMsg::PostedFlattened(flatten_outcome(outcome)))
```

The call-site delta is small but the consumer-side delta is real.
Compare the two `check_*` helpers in `tina_impl.rs`:

```rust
// Layered (shapes 1 and 2):
fn check_layered(outcome: &ReqwestCallOutcome, label: &str) {
    match outcome {
        CallOutcome::Replied(Ok(response)) => { ... }
        CallOutcome::Replied(Err(err)) => panic!("{label} worker: {err}"),
        CallOutcome::Full => panic!("{label} bridge full"),
        CallOutcome::Closed => panic!("{label} bridge closed"),
        CallOutcome::Timeout => panic!("{label} bridge call timed out"),
        CallOutcome::Rejected(reason) => panic!("{label} bridge rejected: {reason:?}"),
    }
}

// Flat (shape 3):
fn check_flat(result: &Result<ReqwestResponse, ReqwestCallError>) {
    match result {
        Ok(response) => { ... }
        Err(ReqwestCallError::Bridge(b)) => panic!("bridge: {b:?}"),
        Err(ReqwestCallError::Worker(e)) => panic!("worker: {e}"),
    }
}
```

Five arms vs three. The flat version *consolidates the bridge-layer
match arms into one branch* without losing the layer name in the
error. That's a real ergonomic win when many call sites all do the
same `bridge_layer ⇒ ... / worker_layer ⇒ ...` dispatch.

### Does flattening feel useful or confusing?

**Useful, but only at the right boundary.**

What worked:
- The flat error type really does preserve the layer
  (`Bridge(BridgeFailure::...)` vs `Worker(ReqwestError::...)`).
  Reading `check_flat` after `check_layered`, the rule "we collapsed
  the match arms but not the meaning" is visible at a glance.
- For app-edge code that maps every failure to "log + give up,"
  three arms beats five.

What felt awkward:
- The continuation-translator syntax for shape 3 is genuinely denser than
  shapes 1 and 2. `.then(DriverMsg::PostedViaSendRequest)` is a
  bare function pointer. `.then(|outcome|
  DriverMsg::PostedFlattened(flatten_outcome(outcome)))` is a
  closure with an inline transformation. A first-time reader will
  look at it twice.
- Mixing all three shapes in one isolate (as we do here, for
  pedagogy) makes the difference visible — but in real code, having
  some call sites layered and some flat without explicit comments
  would be confusing. The choice should be uniform per call site
  cluster, not mixed.
- Naming `PostedFlattened` after the *transformation* rather than
  the *use case* was lazy. A real call site would name the variant
  after what it does next (e.g. `BillingPosted`), which makes the
  flatten step look like a free choice rather than a categorical
  one. The specimen is shouting "this one is the flat one!" at
  the reader.

Net call: keep `flatten_outcome` opt-in and document it as
"per-call-site choice." Don't make it the default. Don't mix
shapes in a single isolate without a comment explaining why. The
crate-level docs in `tina-reqwest-bridge` already say roughly this;
the specimen confirms.

### What was awkward

- The continuation enum has *four* variants (one Run + three
  Posted shapes) just to demonstrate the three call sites. A real
  one-call-shape isolate would have two variants. The pedagogical
  shape is bigger than the natural one.
- The `Driver` isolate's `next_post()` decides which shape to use
  based on `self.counter`. That's a narrative scaffold, not how a
  real isolate would work — it'd pick one shape and stick with it.
- Both sides need a `tokio::runtime::Runtime` (the Tokio side
  hosts reqwest directly; the Tina side gets one from
  `ReqwestWorker::install`). The two-runtimes cost is the bridge's
  nature; it shows up here as in every other bridge specimen.
- For a webhook publisher with many concurrent calls, the
  `max_in_flight` knob on the worker becomes load-bearing.
  `ReqwestConfig::default()` here uses 16 — fine for three
  sequential posts, would need to be tuned for a real fan-out.
