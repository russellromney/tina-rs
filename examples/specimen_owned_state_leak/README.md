# specimen_owned_state_leak

Adversarial probe. Try in good faith to construct a shared-mutable-
state leak through every Tina channel, then document what the type
system blocks vs what user code can still do if it tries.

This specimen has no Tokio comparison — the question is purely about
Tina's "owned state through isolates" claim from the original
FINDINGS.

## Probes

### What the type system blocks

| # | Attempted leak                                      | Compiles? | Why                                          |
|---|-----------------------------------------------------|-----------|----------------------------------------------|
| 1 | `Rc<RefCell<T>>` in a message variant                | No        | `ThreadedRuntime` requires `Message: Send`   |
| 2 | `&mut self.value` moved into a `.reply(...)` closure | No        | reply translator must be `'static`           |
| 3 | `&mut self.value` moved into an outbound `Send` payload closure | No | same `'static` constraint                |
| 4 | Spawn a child whose state contains `Rc<RefCell<T>>`  | No        | `ChildDefinition<I>` requires `I: Send`      |

Each lives as a `compile_fail` doctest in
[`src/tina_impl.rs`](src/tina_impl.rs)'s `compile_fail` module. The
test suite passes when those snippets fail to compile, and would
fail loudly if a future Tina release relaxed any of those bounds.

### What still compiles (the documented anti-pattern)

`Arc<Mutex<T>>` built *outside* the isolate and passed in as an
isolate field compiles and runs. The smoke test exercises this:

```rust
struct Writer {
    shared: Arc<Mutex<u64>>,        // user-built shared state
    remaining: u32,
}
```

The runtime cannot block this — Arc is `Send`, the inner type is
`'static`, the type system has no way to know the user's intent was
to share. **The lesson is that Tina's "owned state" claim is about
what the *isolate model* enforces, not about what the user can
construct.** A user who reaches for `Arc<Mutex<...>>` to share state
between isolates has explicitly opted out of the model.

In practice the right shape is one of:

- `Arc<AtomicU64>` for a coarse counter the host needs to read from
  outside the isolate (existing specimens use this for accounting);
- a publish-back `send` to a known sink isolate (`tina-rpc` and
  `specimen_dynamic_worker_pool` both do this);
- `stop_with(report)` + `runtime.observe_result::<T>(addr)` for the
  isolate's final value.

## Run

```sh
cargo test --manifest-path examples/specimen_owned_state_leak/Cargo.toml
```

The smoke test asserts:

- the runtime probe ran cleanly;
- the user-built `Arc<Mutex<u64>>` did get incremented (proving the
  type system did not block it);
- the documented `compile_fail` count matches the README (the test
  fails if you add a new probe and forget to bump the constant).

```sh
cargo test --doc --manifest-path examples/specimen_owned_state_leak/Cargo.toml
```

runs the four `compile_fail` doctests; each one is a positive test
that the snippet does *not* compile.

## Findings touched

This specimen does not surface a Tina product gap — it documents
the existing type-system contract. The four `compile_fail` doctests
are themselves the evidence; they would start passing-as-compilable
(and therefore failing-as-tests) if a future Tina release relaxed
any of the bounds.

## What this is not

- Not a proof for unsafe Rust escapes. `unsafe { ... }` can always
  defeat the type system. The claim is about safe Rust under the
  Tina API.
- Not a proof for runtime invariants. This specimen tests the
  *type-system* boundary; runtime trace truth is tested in the
  `tina-runtime` crate's own test suite.
