# 022 Closeout

Session:

- A

## Status

Kepler-A closes as settlement by sealing.

No public `tina` boundary change was needed. The implementation keeps the
explicit-step multi-shard model small:

- no shard/peer liveness signal
- no cross-shard child ownership
- no broad runtime allocation-free claim
- no broad cross-shard fault language

## Sealed Rules

### Peer / Shard Liveness

The current multi-shard runtime and simulator do not report distributed
peer/shard liveness. A bad remote address is an address-local failure, not a
destination-shard failure.

Direct proofs:

- `tina-runtime`:
  `cross_shard_unknown_isolate_does_not_poison_destination_shard`
- `tina-sim`:
  `cross_shard_simulation_unknown_isolate_does_not_poison_destination_shard`
- `tina-sim` checker pressure:
  `multishard_checker_accepts_address_local_remote_failure_then_good_traffic`
- `tina-sim` adversarial replay:
  `multishard_checker_failure_replays_for_address_local_liveness_bug`

### Supervision Boundary

Root supervision routes to the owning shard. Children spawned by that parent
belong to the parent's shard. Restart policy applies to direct children on that
same shard. Cross-shard child placement is not expressible in 022.

Direct proofs:

- `tina-runtime`: `multishard_supervision_keeps_children_on_parent_shard`
- `tina-sim`: `multishard_simulation_supervision_keeps_children_on_parent_shard`

### Ownership And Buffering

Cross-shard sends move the user payload into boxed runtime storage when the
effect is erased. The queued remote send owns that boxed payload until
destination harvest moves it into the target mailbox. Core cross-shard
transport does not require user-message cloning.

The only cross-shard coordinator buffer is the bounded shard-pair queue:

- runtime: `BTreeMap<(ShardId, ShardId), VecDeque<QueuedRemoteSend>>`
- simulator: `BTreeMap<(ShardId, ShardId), VecDeque<QueuedRemoteSend>>`

Galileo already directly proves source-time `Full` rejection for that queue.
Kepler carries that as the buffering baseline and narrows the allocation claim.

Allocation proof:

- `tina-runtime`:
  `multishard_runtime_path_still_has_allocations_so_the_claim_stays_narrow`

Allowed allocations/non-claims:

- erased send payload boxes
- boxed handlers / erased call translators / restart recipes
- trace event storage
- replay artifact storage
- coordinator maps and shard-pair queues
- mailbox allocation performed by runtime factory registration

There is no claim that the whole multi-shard runtime/simulator hot path is
allocation-free.

## Verification

`make verify` passed after the implementation.
