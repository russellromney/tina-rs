# 035 Jelle Zijlstra Plan Review 1

Verdict: much sharper and ready to hand to implementation after one explicit
first-step audit. The phase is big, but it is the right bigness: it attacks the
actual missing local I/O substrate instead of wandering into release docs or
random production theater.

## What Looks Strong

- The plan starts from code reality: Tina currently has time and server-side
  TCP, not a full local I/O story.
- It names the surprise missing rock: outbound TCP connect. A service that can
  accept connections but cannot connect outward is not a complete local service
  substrate.
- It uses existing substrate wood correctly: Betelgeuse already has native file
  primitives, while Tina has not exposed them as runtime-owned calls.
- It refuses DNS/TLS/UDP/process/signal as secret scope creep while still
  requiring a final support table with exact reasons.
- It keeps the Tina rule intact: new I/O enters through `RuntimeCall`, uses
  runtime-owned ids, and never leaks raw OS handles or Betelgeuse boxes to
  isolates.
- It requires both live runtime proof and simulator/DST proof for accepted I/O.
- It demands `LocalApp` and bridge-shaped tests, so this does not stop at unit
  plumbing.

## Main Risks

1. **Betelgeuse connect may be the hardest first rock.**
   The current socket trait has bind/accept/recv/send but no connect. Adding a
   completion slot and native backend operation touches the vendored substrate.
   That is correct, but implementation should do this first so the phase does
   not spend days on file polish before discovering connect is awkward.

2. **File simulation can accidentally become persistence.**
   Jelle only needs deterministic local file behavior for config/snapshot/log
   shaped tests. Durable journals, crash recovery, and mailbox persistence
   belong to Wim Kok.

3. **Path/security policy is not a core concurrency primitive.**
   File helpers should accept paths and surface typed outcomes, but they should
   not design sandboxing, permissions policy, or package layout in this phase.

4. **Support table must survive implementation pressure.**
   DNS/TLS/UDP/process/signal should not slide in because they sound
   production-ish. They land only if accepted workloads prove need.

5. **Mailbox fallback language is now fixed, but must stay fixed.**
   The roadmap no longer says `MPSC fallback`; it says mailbox producer model
   decision. That matters. A fallback path hides "main path did not work."
   Jelle must not add a second mailbox route while implementing I/O unless a
   named workload proves the current producer model cannot express the service.
   If multi-producer support ever lands, it must be a first-class bounded
   mailbox implementation with the same visible `Full`/`Closed` contract, not a
   secret escape hatch.

## Hostile Review After Mailbox Wording Fix

Verdict: plan still ready. The fallback smell is removed from both the roadmap
and Jelle rails.

What hostile grug tried to break:

- **"Fallback" hides failure.** Fixed. The roadmap now says one mailbox
  contract and only a possible future bounded multi-producer implementation.
- **Jelle might sneak in MPSC while building file/connect.** Fixed. The plan now
  has a design rule and pause gate: if I/O seems to require multi-producer
  mailbox support, pause and name the workload.
- **Too many mailbox concepts for users.** Still okay because this is not a new
  user surface. The current user promise remains bounded mailbox semantics with
  visible `Full`/`Closed`; implementation choices stay below that contract.
- **Could this block 035?** No. Outbound TCP connect and file I/O should not
  require MPSC. If they do, that is a discovery worth stopping for.

Remaining non-blocking risk:

- The crate layout still lists `tina-mailbox-mpsc` as possible future shape.
  That is acceptable now because it is no longer a fallback and is explicitly
  gated by a named workload.

## Launch Guidance

1. Start with a concrete code audit of `vendor-betelgeuse` socket/file traits,
   native backends, simulated backend, `tina-runtime/src/call.rs`,
   `tina-runtime/src/driver.rs`, and `tina-sim/src/lib.rs`.
2. Implement the smallest Betelgeuse connect proof before touching Tina's
   public call API.
3. After connect is real, thread it through Tina runtime and simulator with one
   user-shaped client workload.
4. Then do file I/O. Keep the simulator file model intentionally small and
   deterministic.
5. Close with the support table and `make verify`.

Grug says: good fire. Start with connect, then files. Do not summon TLS demon.
