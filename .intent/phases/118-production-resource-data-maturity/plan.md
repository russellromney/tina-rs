# Phase 118: Production Resource And Data Maturity

## Status

- Future IDD outline for Wave A.
- Can run in parallel with phases 116 and 117 if ownership stays mostly in pool
  resources, local persistence, and data specimens.

## Purpose

Make long-lived resources and local durable state less first-form.

The user story:

```text
my Tina service can run for a long time, pool resources sanely, and recover
local state after restart
```

## Includes

- pool idle eviction
- pool max lifetime
- pool health check / retire rules
- pool shutdown/drain reports aligned with bridge/resource vocabulary
- DB and HTTP/gRPC client pools using the same pressure language
- snapshot/journal restore service pattern
- torn-write and corrupt-tail recovery specimens
- append-before-apply guard improvements where the type system can help
- persistent keyspace or durable counter system specimen

## Does Not Include

- no distributed consensus
- no durable mailbox
- no exactly-once claim
- no remote clustering
- no hiding database-specific pool truth

## Proof Shape

- idle resource retires and reports why
- max-lifetime retire does not hand stale resource to new caller
- health check retires bad resource
- shutdown drains or force-closes with report
- crash/restart restores expected state
- corrupt/torn journal tail is typed and recoverable
- compile-fail tests prevent apply-before-append helper misuse where practical

