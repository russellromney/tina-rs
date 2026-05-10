# specimen_pool_cancel_reclaim

Cancel a wave of in-flight pool acquires and prove the pool admits new
acquires immediately afterward. The headline 067 capability: caller
cancellation reclaims waiter capacity without a separate
`CancelWaiter` ping.

## Run

```sh
cargo test --manifest-path examples/specimen_pool_cancel_reclaim/Cargo.toml
```

## Tina shape

Driver fans out `WAITERS` parallel `call_with_handle(pool, Acquire,
...)`, stores each `CallHandle`, then on `CancelAll` emits
`cancel_call(handle)` for every parked waiter. The pool's lazy sweep
on the next handler turn reclaims every closed deferred slot. A
follow-up `PressureSnapshot` reads `cancel_count` so the test asserts
on the actual reclaim count, not just timing.

The retry wave that follows must not see `Full` — proof that the
sweep actually freed waiter capacity.

## Tokio shape

`tokio::sync::Semaphore` as the pool. `JoinSet::abort_all` cancels the
parked waiters. The cancelled-counter is `JoinSet::len()` at the
moment of abort.

## What's hard about this in tokio

`Semaphore` has no waiter cap, so "Full" can't naturally happen — we
have to fake it by using `try_acquire_owned` first. The pool's
explicit `max_waiters + Full` outcome is a real invariant; tokio's
backing primitive does not have it.

## What's hard about this in tina

Without a back-channel, a cancel that races the pool's dispatch
(`reply_to(slot, Acquired(lease))` already emitted but rejected
because the caller cancelled before delivery) would leak the resource
forever. The pool's `sweep_in_flight` checks every dispatch's slot
state on each handler turn and reverts `Leased` → `Idle` for any
dispatch whose slot is now `Closed`. The recovered count surfaces in
the pressure report as `dispatch_recovered`.

## Capacity discovery — `unknown -> measured -> fixed`

The pool's waiter cap is a *count* capacity. Pick it once, observe
high water under load, freeze the number. The specimen drives this:

1. **Unknown.** First-pass code reaches for "a big number" — say
   `WAITERS = 4`. Mark it `CapacityMode::Tuning` so the report says
   so out loud.
2. **Measured.** Run the specimen. The driver pulls a
   `PressureReport`, projects it onto the generic capacity surface
   (`pool.demo.waiters`), and emits one discovery line:
   ```text
   pool.demo.waiters        tuning  max=4    cur=0   high=4   full=0    suggest=tuning cap is tight; raise then re-measure
   ```
3. **Fixed.** Read `high` and pick `Fixed(high * safety_factor)`.
   Drop the `Tuning` flag once the cap matches observed pressure.

The `tokio_impl` keeps `waiters_high_water` and `discovery_line`
empty: `tokio::sync::Semaphore` exposes neither a high-water-mark
nor a configured cap, so the same workflow on that side requires
hand-instrumentation. The asymmetry is the point — capacity
visibility is part of Tina's discipline, not an add-on.
