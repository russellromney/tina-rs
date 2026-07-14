# specimen_pool_cancel_reclaim

Cancel a wave of in-flight pool acquires and prove the pool admits new
acquires immediately afterward. The headline capability: caller
cancellation reclaims waiter capacity without a separate
`CancelWaiter` ping.

## Run

```sh
cargo test --manifest-path examples/specimen_pool_cancel_reclaim/Cargo.toml
```

## Tina shape

The host sends one `BeginPrime`; the driver owns the rest of the
control sequence. It fans out `WAITERS` parallel
`call_cancelable(pool, Acquire, ...)` calls, stores each `CallHandle`
in a `PendingCallSet` bounded to `WAITERS`, and uses a typed actor-owned
timer before emitting `cancel_call(handle)` for every parked waiter.
After every cancellation acknowledgement arrives, the driver requests
the pool pressure report. Only after that call settles does it launch
the retry wave, and a second typed timer releases the prime lease.

The retry wave that follows must not see `Full` — proof that the
sweep actually freed waiter capacity.

The driver reports only after all cancel acknowledgements, the
pressure snapshot, every retry result, and every lease release have
settled. Exact cancel, acquire, release, pressure, and control-timer
terminals remain in the public report; no host sleep is used as proof.

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

The waiter cap is a count cap. Pick a number, run load, read
high water, freeze. The specimen does it:

1. **Unknown.** Pick `WAITERS = 4`. Mark it `Tuning`.
2. **Measured.** Driver reads `PressureReport`, projects it onto
   the count surface, prints one line:
   ```text
   capacity surface=pool.demo.waiters mode=tuning max=4 cur=0 high=4 full=0 suggest="tuning cap is tight; raise then re-measure"
   ```
3. **Fixed.** Read `high`. Pick `Fixed(high * safety_factor)`.
   Drop the `Tuning` flag.

The tokio side leaves `waiters_high_water` and `discovery_line`
empty. `tokio::sync::Semaphore` does not expose live waiters,
high water, or a configured cap. Same workflow needs hand
instrumentation there.
