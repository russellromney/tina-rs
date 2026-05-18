# Phase 121: Ecosystem Hooks And Async Boundary

## Status

- Future IDD outline for Wave B.
- Can run in parallel with phases 119 and 120 if ownership stays in public hook
  traits, extension smoke crates, capability reports, and docs.

## Purpose

Let Tina grow an ecosystem without every feature landing in core.

The user story:

```text
I can plug in a codec, bridge, capacity surface, event sink, or policy without
private runtime access and without weakening bounded/DST truth
```

## Includes

- public capacity surface hook
- bounded event sink hook
- sync codec adapter hook
- service policy hook
- bridge author smoke crate after Phase 113
- fake external bridge smoke crate using only public APIs
- custom codec smoke crate using only public APIs
- runtime capability report for rails/cancel/drain/sim support
- clear async boundary docs:
  - native Tina path
  - bridge path
  - unsupported path

## Does Not Include

- no dynamic plugin ABI
- no broad `Future`/`Stream` bridge unless a smoke crate proves the bounded
  shape
- no hidden Tokio under native Tina services
- no hook that bypasses trace/capacity/cancel truth

## Proof Shape

- extension smoke crates compile and run using only public APIs
- custom surface joins normal capacity summary
- event sink is bounded and reports drops/full/closed
- codec hook keeps parser state replayable
- capability report says supported/unsupported/cancel/drain/sim truth
- compile-fail tests prevent hooks from constructing invalid private runtime
  state

