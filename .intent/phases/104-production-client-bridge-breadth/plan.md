# Phase 104: Production Client / Bridge Breadth

## Status

- IDD implementation phase.
- Not started.

## Grug Truth

Production Tokio apps talk to other systems.

Tina does not need every SDK feature, but it needs boring, bounded first-real
paths for the services people reach for every week.

## Goal

Broaden the bridge/client surface without hiding external-system truth:

- AWS bridge grows beyond the first service.
- DNS/connect policy is visible.
- Bridge lifecycle and metrics stay consistent.
- Late external work is never claimed as cancelled unless Tina can prove it.

## Non-Goals

- No generic AWS framework.
- No hidden retry.
- No hidden idempotency.
- No fake cancellation of external SDK work.
- No shared bridge base crate unless three bridges prove the same code shape.

## Rocks

### Rock 1: AWS Service Breadth

Add the next AWS operations by need, not ambition:

- DynamoDB: get/put/query/update/delete with typed capacity/error facts.
- SNS: publish with typed terminal outcome.
- Secrets Manager: get secret value with typed size/error caps.
- SQS/S3 follow-ups only where the first AWS bridge left obvious gaps.

Each operation has:

- request cap
- response cap
- timeout
- worker-terminal metrics
- caller-visible timeout/cancel truth
- tracing fields with operation and service name

### Rock 2: DNS and Connect Policy

Make outbound connection policy visible:

- connect timeout
- DNS timeout
- DNS failure
- TLS name/cert failure
- connect racing only if the runtime can name the winner/loser truth

Do not bury connect behavior inside protocol clients.

### Rock 3: Bridge Lifecycle Convention

Audit bridge install/close/drain/metrics/config validation across:

- reqwest
- sqlite
- sqlx
- AWS
- tokio/tower/rpc bridges

Fix real mismatches. Document deliberate differences.

### Rock 4: Supplied Client Ownership

For every `install_with_client` / `install_with_pool` style API:

- name who owns runtime/task/thread lifetime
- name who owns timeouts
- name who owns retry
- name what happens on close
- reject config that is ignored, or say clearly it is ignored

### Rock 5: Bridge Error Classifiers

Add tiny classifiers only where they are obvious:

- success
- transient
- fatal
- caller timeout
- caller cancelled
- external terminal after abandonment

No broad policy object. The caller decides retry safety.

## Required Proof

- Targeted tests for every new operation.
- Close with in-flight work.
- Caller timeout followed by late external completion when observable.
- Supplied-client config ownership tests.
- Tracing tests for operation/service fields.
- Capacity reports cannot lie about installed capacity.
- Docs show copied paths, not raw internal messages.

## Done Means

A user building a normal service can call DB, HTTP, and the common AWS services
through bounded Tina-shaped bridges, and every timeout/cancel/late-result story
is named honestly.
