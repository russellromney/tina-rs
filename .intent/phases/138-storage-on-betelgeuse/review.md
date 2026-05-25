# Phase 138 Review (append-only)

## Plan Review 1 — hostile (2026-05-24)

Verdict: premise is solid and verified. Two real correctness holes in the proof,
one internal contradiction, and the "inline fallback" lean is more dangerous than
the plan admits.

### Finding 1 (blocking) — the proof does not actually prove write→fsync ordering

With std::fs, write then fsync are sequential on one thread, so ordering is free.
On io_uring/Betelgeuse you submit `pwrite`, get its completion, **then** submit
`fsync`. The hazard is submitting `fsync` before the `pwrite` completion is
harvested. The plan's Hard Constraint 3 says "wait for the fsync completion before
applying state" — but the real ordering rule is **"wait for the pwrite completion
before submitting fsync."** A happy-path round-trip test PASSES even when this is
racy (bytes are usually there). "How could this be broken while tests pass?" is
trivially answerable → the proof is weak.

**Required plan change:** add a fault-injection proof using the Betelgeuse
simulated backend with delayed/reordered completions, asserting that
`apply` never observes state whose backing `pwrite`+`fsync` have not both
completed in order. Happy-path round-trip is `surrogate proof` here, not direct.

### Finding 2 (blocking) — `CommitUncertain` reproduction has no injection mechanism

On the new path, snapshot commit spans **two mechanisms**: a fallback `rename`
(not Betelgeuse) + a Betelgeuse parent-dir `fsync`. The plan says
`CommitUncertain` is "reproduced when the final durability step cannot be proven"
but never says **how to inject** that failure on the new path. Without an
injection hook it is `missing proof`. **Required plan change:** name the injection
points — Betelgeuse simulated fsync-failure for the fsync leg, and a fault hook on
the fallback rename — and assert `CommitUncertain` arises from each.

### Finding 3 (blocking) — the guard contradicts the open fallback decision

Proof bullet: "no `thread::spawn` for the storage durability path; a thread-count
assertion shows no storage worker." Open Decision: the rename/remove/readdir/
metadata fallback might be "a tiny bounded syscall **worker** thread." These
conflict — if the fallback is a worker, the guard cannot assert "no storage
thread." **Required plan change:** resolve the fallback mechanism *in this plan*,
then make the guard match it. If the fallback stays a worker, the guard must be
"no thread for the Betelgeuse-supported ops," not "no storage thread at all."

### Finding 4 — "inline on shard" for the fallback is a TPC regression in disguise

The plan leans toward running rename/remove/readdir/metadata **inline on the shard
thread** because they are "rare/fast." But `rename` (and especially the snapshot-
commit rename + parent fsync) can block — and blocking the shard thread is the one
thing thread-per-core forbids. "Rare but blocking on the shard" is exactly the
footgun this whole TLS/storage workstream is removing. **Recommendation:** keep
the lacking-op fallback **off the shard** (thin bounded worker) until/unless
`renameat`/`unlinkat` are upstreamed into Betelgeuse. Do not trade a clean
off-shard worker for an on-shard stall to win a "zero threads" headline.

### Finding 5 — clarify the Inline-vs-live boundary

Constraint 2 says the oracle keeps `Inline` and the live path changes. Confirm
`StorageLane::Inline` is **only** ever the explicit-step runtime and never a live
`LocalSystem`/`ThreadedRuntime` config, so "live changes / oracle unchanged" is
unambiguous. One sentence in Starting Facts.

### Keep

The Betelgeuse-op vs fallback-op split, recovery-semantics-preserved constraint,
and the oracle-untouched mirror of the TLS split are all right.

## Plan Review 2 — second reviewer (2026-05-25)

Verdict: the fallback-worker decision is right; the headline proof wording still
needed to match it.

### Finding 1 — do not claim "no storage worker" while keeping a fallback worker

The plan correctly keeps rename/remove/readdir/metadata off the shard on a tiny
bounded fallback worker. One proof bullet still said "no storage worker thread
spawned." Fixed in plan v3: the direct proof is "no worker thread for
Betelgeuse-supported durability ops"; the metadata fallback worker is allowed
and named.
