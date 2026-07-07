# Continuation Flows

Use `tina::flow!` for fixed multi-step request handlers.

It writes the continuation enum and dispatch method you would otherwise write
by hand. It does not change Tina's runtime contract: each runtime call still
returns later as one ordinary message, each step still receives the full
`CallOutcome<T>`, and caller authority still moves as `RequestContext<R>`.

## Shape

Add one wrapper variant to your message enum:

```rust
enum ControllerMsg {
    Http(HttpRequest),
    Notify(NotifyFlow),
}
```

Declare the flow near the message enum:

```rust
tina::flow! {
    flow NotifyFlow for Controller {
        reply HttpResponse;

        step Loaded(scope_id: u64, id: i64, slow: bool) -> SqliteResult {
            match item_from_rows(id, outcome) {
                Ok(Some(name)) => call(self.pool, WorkerPoolMsg::Acquire, REQUEST_TIMEOUT)
                    .then_with_request(req, move |req, outcome| {
                        ControllerMsg::Notify(NotifyFlow::Acquired(
                            req, scope_id, id, name, slow, outcome,
                        ))
                    }),
                Ok(None) => reply_to(req, text(StatusCode::NOT_FOUND, "not_found\n")),
                Err(response) => reply_to(req, *response),
            }
        }

        step Acquired(scope_id: u64, id: i64, name: String, slow: bool)
            -> WorkerPoolReply<KeepaliveConnAddr>
        {
            match outcome {
                CallOutcome::Replied(WorkerPoolReply::Acquire(AcquireOutcome::Acquired(lease))) => {
                    let request = HttpRequest::post("/notify")
                        .text_body(format!("id={id}&name={name}"))
                        .build();
                    // The outbound request call is the request's cancelable
                    // child: register it into the scope so a scope cancel
                    // closes the parked wait.
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
                other => reply_to(req, pool_acquire_error_response(other)),
            }
        }
    }
}
```

Dispatch the wrapper from `handle`:

```rust
ControllerMsg::Notify(flow) => self.handle_notify_flow(flow),
```

Start the first step from a call handler with the normal request-authority
spelling:

```rust
call_ctx
    .defer(send_request(self.db, query, REQUEST_TIMEOUT))
    .reply(move |req, outcome| {
        ControllerMsg::Notify(NotifyFlow::Loaded(req, scope_id, id, slow, outcome))
    })
```

## Generated Code

The expansion is intentionally boring:

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

impl Controller {
    fn handle_notify_flow(&mut self, msg: NotifyFlow) -> Effect<Self> {
        match msg {
            NotifyFlow::Loaded(req, scope_id, id, slow, outcome) => { /* body */ }
            NotifyFlow::Acquired(req, scope_id, id, name, slow, outcome) => { /* body */ }
        }
    }
}
```

Step names become enum variant names. Captured state is exactly the field list
you wrote. `req` is always the first field and `outcome` is always the last
field. The generated handler has the same visibility as the flow enum.

## Rules

- Each step body must mention `req`. Reply with it, thread it into the next
  step, or explicitly drop it. A `req` binding introduced inside a closure,
  match arm, or local pattern does not satisfy this rule.
- Step bodies use the caller crate's normal lint level. The macro does not
  upgrade unused user locals or captures to hard errors.
- Match `CallOutcome<T>` in the step body. `Full`, `Closed`, `Timeout`,
  `Rejected`, and domain errors inside `Replied(...)` are still application
  decisions.
- Keep cancelable admission explicit. If a step starts `call_cancelable`, store
  the returned handle in bounded state before returning the effect.
- Do not use a flow for fan-out, joins, or open-ended loops. Use explicit state,
  `CallGroup`, or a hand-written state machine.
- Do not batch multiple calls against the same runtime-owned resource in one
  step.

If the using crate depends on Tina under different crate names or exposes
crate-root aliases, configure the generated paths before `reply`:

```rust
tina::flow! {
    flow NotifyFlow for Controller {
        tina_crate = ::my_tina;
        runtime_crate = ::my_tina_runtime;
        reply HttpResponse;

        step Loaded(id: i64) -> SqliteResult {
            /* ... */
        }
    }
}
```

## When To Hand-Write

Hand-write the continuation enum when the workflow is not mostly linear, when
step names need custom compatibility behavior, or when a branch owns complex
bounded state. `tina::flow!` is only the default for the common "do one runtime
call, inspect outcome, dispatch the next runtime call" request path.
