# Phase 119: Production Resource And Data Maturity

## Status

- Future IDD outline for Wave A.
- Runs after Phase 116. HTTP/2/gRPC client resource maturity needs the real
  client connection and stream-slot shape first.
- Can run in parallel with later non-pool work if ownership stays mostly in
  pool resources, local persistence, and data specimens.

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
- DB, HTTP/1 keepalive, and HTTP/2/gRPC client resources using the same
  pressure language without pretending every resource is one lease per request
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
- no redesign of HTTP/2/gRPC client protocol state; Phase 116 owns that

## Proof Shape

- idle resource retires and reports why
- max-lifetime retire does not hand stale resource to new caller
- health check retires bad resource
- shutdown drains or force-closes with report
- HTTP/2/gRPC client connection retire does not kill unrelated healthy
  connection state silently
- crash/restart restores expected state
- corrupt/torn journal tail is typed and recoverable
- compile-fail tests prevent apply-before-append helper misuse where practical
