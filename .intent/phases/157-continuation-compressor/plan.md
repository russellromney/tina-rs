# Continuation Compressor

## Problem

Multi-step Tina request handlers are correct but too loud. A service that does
`load row -> acquire lease -> outbound call -> release lease -> reply` writes
one continuation message variant and one handler arm per step. The runtime
contract is already the right contract: every suspension point is a message,
every wait reports `CallOutcome`, and caller authority is carried explicitly as
`RequestContext`. The missing piece is authoring sugar that writes the same
state machine a careful human would write.

## Candidate Surfaces

All examples spell the same `POST /items/{id}/notify` flow from
`examples/systems/mini_saas_api`.

### A. Function-like item macro

```rust
enum ControllerMsg {
    Http(HttpRequest),
    Notify(NotifyFlow),
    /* existing variants */
}

tina::flow! {
    flow NotifyFlow for Controller {
        reply HttpResponse;

        step Loaded(scope_id: u64, id: i64, slow: bool) -> SqliteResult {
            match item_from_rows(id, outcome) {
                Ok(Some(name)) => {
                    call(self.outbound_pool, WorkerPoolMsg::Acquire, REQUEST_TIMEOUT)
                        .then_with_request(req, move |req, outcome| {
                            ControllerMsg::Notify(NotifyFlow::Acquired(
                                req, scope_id, id, name, slow, outcome,
                            ))
                        })
                }
                Ok(None) => {
                    self.retire_scope(scope_id);
                    reply_to_request(req, text(StatusCode::NOT_FOUND, "not_found\n"))
                }
                Err(response) => {
                    self.retire_scope(scope_id);
                    reply_to_request(req, *response)
                }
            }
        }

        step Acquired(scope_id: u64, id: i64, name: String, slow: bool)
            -> WorkerPoolReply<KeepaliveConnAddr>
        {
            match outcome {
                CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => {
                    self.outbound_acquired += 1;
                    let body = if slow {
                        format!("id={id}&name={name}&slow=true")
                    } else {
                        format!("id={id}&name={name}")
                    };
                    let request = HttpRequest::post("/notify").text_body(body).build();
                    let (effect, handle) = call_cancelable(
                        *lease.handle(),
                        KeepaliveConnectionMsg::request(request, REQUEST_TIMEOUT),
                        REQUEST_TIMEOUT + Duration::from_secs(1),
                    )
                    .then(move |outcome| {
                        ControllerMsg::Notify(NotifyFlow::Sent(req, scope_id, lease, outcome))
                    });
                    if let Some(scope) = self.notify_scopes.get(&scope_id) {
                        scope.register("outbound_request", handle)
                            .expect("fresh notify scope has room for the single outbound child rail");
                    }
                    effect
                }
                other => {
                    self.retire_scope(scope_id);
                    reply_to_request(req, pool_acquire_error_response(other))
                }
            }
        }

        step Sent(scope_id: u64, lease: PoolLease<KeepaliveConnAddr>) -> KeepaliveOutcome {
            let (ok, disposition) = match &outcome {
                CallOutcome::Replied(KeepaliveOutcome::Request { result: Ok(response), .. }) => {
                    (response.status.is_success(), ReleaseDisposition::Reuse)
                }
                _ => (false, ReleaseDisposition::Retire),
            };
            call(
                self.outbound_pool,
                WorkerPoolMsg::Release { lease, disposition },
                REQUEST_TIMEOUT,
            )
            .then_with_request(req, move |req, release| {
                ControllerMsg::Notify(NotifyFlow::Released(req, scope_id, ok, release))
            })
        }

        step Released(scope_id: u64, ok: bool) -> WorkerPoolReply<KeepaliveConnAddr> {
            self.retire_scope(scope_id);
            match &outcome {
                CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) => {
                    self.outbound_released += 1;
                }
                CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Retired)) => {
                    self.outbound_retired += 1;
                }
                _ => {}
            }
            match outcome {
                CallOutcome::Replied(WorkerPoolReply::Release(ReleaseOutcome::Released)) if ok => {
                    reply_to_request(req, text(StatusCode::OK, "notified\n"))
                }
                CallOutcome::Replied(WorkerPoolReply::Release(_)) if ok => {
                    reply_to_request(req, text(StatusCode::SERVICE_UNAVAILABLE, "outbound_release\n"))
                }
                _ => reply_to_request(req, text(StatusCode::BAD_GATEWAY, "notify_failed\n")),
            }
        }
    }
}
```

The start site remains explicit:

```rust
call.defer(send_request(...)).reply(move |req, outcome| {
    ControllerMsg::Notify(NotifyFlow::Loaded(req, scope_id, id, slow, outcome))
})
```

### B. Attribute macro on an inherent impl

```rust
#[tina::flow(message = ControllerMsg::Notify)]
impl Controller {
    #[step(reply = HttpResponse)]
    fn notify_loaded(
        &mut self,
        req: RequestContext<HttpResponse>,
        scope_id: u64,
        id: i64,
        slow: bool,
        outcome: CallOutcome<SqliteResult>,
    ) -> Effect<Self> {
        /* same body */
    }
}
```

The macro would generate `NotifyFlow` from annotated methods. This looks like
ordinary Rust but splits the flow across several functions, making the linear
chain harder to scan and making generated names less obvious.

### C. Typed step builder

```rust
let flow = Flow::<Controller, HttpResponse>::new("Notify")
    .step::<SqliteResult>("Loaded", |state, req, (scope_id, id, slow), outcome| { /* ... */ })
    .step::<WorkerPoolReply<KeepaliveConnAddr>>("Acquired", |state, req, captures, outcome| { /* ... */ });
```

This avoids proc-macro parsing, but it cannot generate enum variants. Captured
state would have to become boxed trait objects or user-written structs, which
is less debuggable than the manual enum and worse for trace readability.

## Chosen Surface

Choose A: `tina::flow!`.

Reasons:

- Generated code is predictable: an enum plus one `match`, with variant names
  matching step names.
- Captured state is explicit in the macro input and in the generated enum.
- The macro does not need to rewrite an existing message enum. The user adds a
  wrapper variant like `ControllerMsg::Notify(NotifyFlow)`, which is additive.
- Expansion looks like hand-written continuation code and survives
  `cargo expand` review.
- The start site still consumes `CallContext` through `defer(...).reply(...)`,
  preserving the existing authority rules.

## Generated Code

For each step:

```rust
enum NotifyFlow {
    Loaded(RequestContext<HttpResponse>, u64, i64, bool, CallOutcome<SqliteResult>),
    Acquired(
        RequestContext<HttpResponse>,
        u64,
        i64,
        String,
        bool,
        CallOutcome<WorkerPoolReply<KeepaliveConnAddr>>,
    ),
}
```

The macro also emits:

```rust
impl Controller {
    fn handle_notify_flow(&mut self, msg: NotifyFlow) -> Effect<Self> {
        match msg {
            NotifyFlow::Loaded(req, scope_id, id, slow, outcome) => { /* body */ }
            NotifyFlow::Acquired(req, scope_id, id, name, slow, outcome) => { /* body */ }
        }
    }
}
```

The message dispatcher stays explicit:

```rust
ControllerMsg::Notify(flow) => self.handle_notify_flow(flow),
```

## Error Arm Ergonomics

Each step body receives the full `CallOutcome<T>` as `outcome`. That keeps all
runtime overload outcomes visible: `Full`, `Closed`, `Timeout`, `Rejected`,
and domain errors inside `Replied(...)`.

The first implementation deliberately does not infer default mappings. A later
additive extension can support:

```rust
step Acquired(...) -> WorkerPoolReply<KeepaliveConnAddr>
    else |req, other| {
        self.retire_scope(scope_id);
        reply_to_request(req, pool_acquire_error_response(other))
    }
{
    let CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) = outcome;
    /* success body */
}
```

The initial form is noisier but avoids hiding Tina's overload vocabulary.

## Deliberately Not Supported

- Loops. Use an explicit helper state in the isolate and re-enter the same
  generated step from the body.
- Branch fan-out or join. Use `CallGroup`, `PendingCancelableCallSet`, or a
  purpose-built state machine.
- Hidden cancellation admission. A step may call `call_cancelable`, but storing
  the handle into bounded state remains user code.
- Batch synthesis. A step may return `batch(...)`, but the macro will not
  combine calls.
- Automatic final replies. The final step must call `reply_to_request` or
  otherwise intentionally consume/drop the `RequestContext`.

## Reviewable Diff Strategy

Port one flow only: mini SaaS `POST /items/{id}/notify`. Keep the old
`NotifyLoaded` / `NotifyAcquired` / `NotifySent` / `NotifyReleased` variants
in the enum during this PR so parallel work and downstream references do not
break. The route will start the new `NotifyFlow`, and `handle` will dispatch
the new wrapper variant.
