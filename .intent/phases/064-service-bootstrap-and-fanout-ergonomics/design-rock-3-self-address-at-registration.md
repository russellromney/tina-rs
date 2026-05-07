# Rock 3 Design Note — Self-Address At Registration

## Status

Design + minimal implementation. Single-shard runtimes only in
064; multi-shard parity is a follow-up if a multi-shard example
needs it.

## Problem

A coord wants its own typed `Address` so it can hand it to
children, register an observer, or wire a reply adapter. Today
the workaround is a `Begin { self_addr }` (or `Bind { self_addr }`)
message whose only job is to land the address into state:

```rust
// eiffel_dynamic_worker_pool today
let coord_addr = runtime.register_with_capacity::<_, Infallible>(coord, cap)?;
runtime.try_send(coord_addr, CoordMsg::Begin { self_addr: coord_addr })?;
```

The handler then matches on `Begin { self_addr }`, stashes the
address, and runs the actual first-turn work. The variant is pure
ceremony.

## Candidate API

```rust
pub fn register_with_capacity_using<I, Outbound, F>(
    &mut self,
    mailbox_capacity: usize,
    construct: F,
) -> Address<I::Message, I::Reply>
where
    I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
    F: FnOnce(Address<I::Message, I::Reply>) -> I,
    /* same erasure bounds as register_with_capacity */
```

Mirror on the threaded shell:
`ThreadedRuntime::register_with_capacity_using(cap, |self_addr| ...)`.

Multi-shard parity (deferred):
`MultiShardRuntime::register_with_capacity_using_on(shard, cap, ...)`,
`ThreadedMultiShardRuntime::register_with_capacity_using_on(...)`,
and the simulator equivalent. The shapes are mechanical
copies of the single-shard form, but each runtime requires its
own panic/escape tests; do not ship a partially-tested
multi-shard form.

## Settled Questions

### When is the address generation allocated?

Before `construct` runs. The runtime allocates `IsolateId` and
`AddressGeneration::new(0)` first, builds the typed
`Address::new_with_generation(...)`, then calls `construct`. The
generation is always the initial 0, matching plain
`register_with_capacity`.

### Can messages deliver before the constructor returns?

No. Mailbox creation happens before `construct`, but the
`RegisteredEntry` is not pushed into `self.entries` until after
`construct` returns. Any `try_send` against the address from
inside `construct` would not find the entry — see the escape
section below.

### Constructor panics

`next_isolate_id` was bumped; no entry was pushed. The id is
leaked (never reused), the mailbox is dropped on unwind, and the
panic propagates to the caller. The runtime's id allocator is
monotonic in normal use, so leaking one id is harmless.

There is no `IsolateRegistered` trace event for a panicked
construction — emission happens after entry push, not before.

### Constructor returns error

The fallible form is not in 064. If a future phase wants it,
shape:

```rust
pub fn try_register_with_capacity_using<I, Outbound, F, E>(
    &mut self,
    mailbox_capacity: usize,
    construct: F,
) -> Result<Address<I::Message, I::Reply>, E>
where F: FnOnce(Address<I::Message, I::Reply>) -> Result<I, E>;
```

Same id-leak / no-entry / no-trace semantics on `Err`.

### Can self_addr escape if registration fails?

The constructor's only input is the typed `Address`. The closure
has no `&self` on the runtime, so it cannot call `try_send` or
`register_*` from inside `construct`. The address can leak only
through state captured by the closure — e.g. an
`Arc<Mutex<Option<Address>>>` that the user explicitly wired in
to share the address with another thread.

That escape is *user-visible* shared state, not a hidden runtime
back-channel. If the user does it and `construct` panics, the
escaped address points at an id with no entry. Behavior at
`try_send`:

- Single-shard explicit-step `Runtime::try_send`: panics with
  `runtime ingress targeted unknown isolate {N}`.
- Threaded runtime: same panic, on the worker thread, surfacing
  through the runtime command.

Both panics are loud. We do **not** treat
"ingress to id with no entry" as a typed `Closed` return — that
would mask programmer error in the dominant case (typo, dropped
runtime, address from another shard). A future phase that wants
to avoid the unknown-id panic for this exact constructor-failure
case should add a tombstone / failed-registration state and a
typed `RegistrationFailed` outcome. That is a different runtime
model: the runtime would keep a record for an isolate that never
registered. 064 does not take that on.

Rule for this helper:

> The runtime must not create a hidden usable address after failed
> registration. Explicit user-shared escape is still possible in
> Rust, but it is a loud programmer error, not a live or silently
> closed address.

### Constructor sends self_addr to another isolate before
registration commits

The closure cannot send anything itself — it has no access to
the runtime. If the user writes a constructor that *spawns a
thread* and then in that thread calls `runtime.try_send(self_addr,
...)`, the sequencing is:

1. `register_with_capacity_using` allocates id, builds
   `self_addr`, calls `construct`.
2. `construct` spawns thread, returns `I`. The thread holds
   `self_addr` and a runtime handle.
3. Main thread pushes the entry. After this point the address
   is live.
4. The spawned thread races with step 3. If its `try_send` lands
   before step 3, it panics (unknown isolate). If after, it
   delivers.

This race is user-visible and avoidable: the user controls the
spawned thread. The runtime does not promise a window in which
late-arriving sends to a partially-registered isolate are
buffered. Document the rule:

> The address handed to `construct` is live for delivery only
> after `register_with_capacity_using` returns. Sends from a
> thread the constructor itself spawned must wait on a barrier
> the user owns, not on the runtime.

### Trace events

- One `IsolateRegistered { isolate_id, generation, parent: None }`
  event after entry push, identical to plain
  `register_with_capacity`.
- No event before construct runs. No event on construct panic.

### Explicit-step and threaded parity

- Explicit-step `Runtime::register_with_capacity_using`: ships
  in 064.
- `ThreadedRuntime::register_with_capacity_using`: ships in 064;
  delegates to the inner runtime via the existing command
  channel. The constructor runs *on the worker thread*, not the
  caller; the worker thread holds the runtime mutably during
  registration. Caller blocks for the duration.
- `MultiShardRuntime::register_with_capacity_using_on`: deferred.
- `ThreadedMultiShardRuntime::register_with_capacity_using_on`:
  deferred.
- `MultiShardSimulator::register_with_capacity_using_on`:
  deferred.

The deferred forms are not blocked on a design problem; they are
deferred because no in-tree multi-shard example needs them yet,
and shipping each one demands its own panic / escape /
parity-with-explicit-step test pass. Add them with the next
multi-shard example that motivates the helper.

### Macro interaction

`#[tina_runtime::isolate]` only emits the `Isolate` trait impl
on the user's struct. It does not generate registration code.
The helper is therefore independent of the macro and works on
plain `impl Isolate` types too.

## Rule Check

- "no hidden usable address after failed registration" — failure
  cases do not push an entry; explicit user-shared escape panics
  loud on `try_send`.
- "no hidden first message" — the helper has no
  `with_initial_message` analog. If the user wants the first
  message to land, they call `try_send` after the helper
  returns.
- "no delivery until registration is complete" — entry is pushed
  only after `construct` returns. The address is not in the
  registry table until step 3.
- "address generation semantics match normal registration" —
  `AddressGeneration::new(0)`, identical to plain `register_*`.
- "panic/failure cleanup is tested" — see the test list below.

## Tests Shipped In 064

- `register_with_capacity_using_returns_address_with_initial_generation`
- `register_with_capacity_using_constructor_receives_self_address`
- `register_with_capacity_using_no_message_delivers_before_construct_returns`
- `register_with_capacity_using_panic_in_constructor_propagates_and_pushes_no_entry`
- `register_with_capacity_using_panic_does_not_reuse_id`
- threaded mirror: same panic and id-leak shape via the command
  channel.

## Migration

`eiffel_dynamic_worker_pool`'s `CoordMsg::Begin { self_addr }` is
the textbook case: the variant exists only to land the address
into coord state. After this rock, the example registers the
coord with a `|self_addr| Coordinator { self_addr, ... }`
constructor and the host kicks the work with a typed
`CoordMsg::Start` that no longer carries an address.

`eiffel_sharded_fanout_read`'s `Bind { bridge }` is **not**
migrated by this rock. The coord still needs the *adapter's*
address, which is allocated separately and is not the coord's
self-address. Self-address-at-registration solves only the
self-address half of finding 3; cross-isolate handshake still
needs `Bind` or a future paired-registration primitive.
