# Battery Authoring

This page is for someone writing or reviewing a battery — a blessed Tina
crate that adds a protocol, bridge, codec, or runtime helper on top of core
Tina. It is the short checklist. For the deeper bridge-specific copy path
see [30-bridge-author-kit.md](30-bridge-author-kit.md). For the core-vs-
batteries layering, see [23-core-and-batteries.md](23-core-and-batteries.md).

The audience is first-party and third-party battery authors. The same rules
apply to both. Calling a battery "official" is just a label; the rules are
the same.

## The job

You are adding a capability on top of Tina without inventing new core
semantics. Your battery must:

- plug into existing public hooks (runtime rails, bridge install/closer
  traits, capacity surfaces, replay facts);
- keep Tina's bounded, typed, visible-outcome discipline;
- name any gap honestly rather than papering over it.

## Checklist

Each item below is a yes/no question. If you cannot answer yes, the battery
is not done — either fix it, or name the gap in the table at the bottom.

1. **Public hooks only.** Do you use only `tina::*`, `tina_runtime::*`
   public APIs? No reaching into `runtime_internal` or private modules?
2. **Bounded admission.** Does every queue, pool, and worker channel have an
   explicit cap from a typed config? Is the cap validated up front?
3. **Typed outcomes.** Does every operation that can refuse return a typed
   enum that names `Full` / `Closed` / `Timeout` / domain errors? No
   stringly-typed outcomes?
4. **Closer and drain.** Does your battery expose a `BridgeCloser` (or
   equivalent) that names exactly what `close()` does and what `drain`
   reports? Is `close()` idempotent?
5. **Pressure report.** Do you produce a `BridgePressure`-shaped (or
   battery-specific) report with `current`, `high_water`, `cap`, and
   `mode`? Does it join the runtime's normal capacity summary?
6. **Trace truth.** Do worker-terminal facts emit through public runtime
   events (or `Effect::Fact` where applicable)? No silent state changes?
7. **Cancellation.** Does the battery respect runtime cancellation? On
   cancel, does the caller observe a typed `Cancelled` outcome and does
   late work get counted (`late_results`)?
8. **Replay story.** Does your battery either support `tina-sim` replay, or
   declare itself unsupported / projection-only, so a saved replay case
   fails closed instead of silently passing?
9. **No hidden Tokio queues.** If you wrap a Tokio-shaped SDK, is the
   bounded queue between caller and worker visible? Are oneshots or
   `mpsc::unbounded` channels absent or justified?
10. **Compile-time rails.** Does at least one `trybuild` fixture pin the
    misuse case (wrong effect family, missing trait, leaked private type)?
11. **Smoke user.** Does at least one specimen exercise the battery on the
    happy path and at least one bounded-failure path?
12. **Doc page.** Does the battery have a short user-guide page that names
    its closer, its pressure shape, its outcome enum, and one paragraph on
    replay support?

If a third-party battery author cannot satisfy item 1 ("public hooks only")
without reaching into runtime internals, that is a Tina core bug — either
promote the missing surface to a public hook in a future phase, or list the
gap in the table below.

## Known hook gaps

Today's first-party batteries already lean on a few cracks that are not yet
clean public hooks. Naming them honestly here lets new battery authors know
what they will hit, and lets future phases close them on purpose.

| Gap | Status today | Path forward |
|---|---|---|
| **HTTP/TLS rails** | `tina-runtime`'s TCP/TLS rail builders are mostly used by first-party `tina-http`. Third-party protocol crates can reach them, but the contract is still implicit ("read the runtime call types") rather than a published surface. | A future phase publishes a small "protocol rail" trait and a guide. |
| **Bridge lifecycle** | `BridgeInstall`, `BridgeCloser`, `BridgePressure`, `BridgeOutcomeClass` exist and are public. There is no shared author kit crate yet: each bridge re-implements the same plumbing by copy. | Promote shared lifecycle helpers into `tina-runtime::bridge` (no framework blob) once two third-party bridges have copied the same shapes. |
| **Body streaming / source lifecycle** | Tina-owned chunked body sources work inside `tina-http`. There is no public "body source" battery protocol yet — third-party HTTP-shaped batteries would have to re-derive the source lifecycle. | A future phase extracts a typed body-source surface used by both `tina-http` and any new protocol battery. |
| **AWS / sqlx / reqwest / Tokio-owned workers** | The bridge pattern (closer, metrics, classifier, late-result counter) is copied by hand in each crate. Worker plumbing is similar enough to refactor, but not yet shared. | After Wave A, promote a single bridge author kit crate; current copies become thin shims. |
| **Replay support per battery** | `tina-http` HTTP/2 / gRPC / WebSocket facts now ride `Effect::Fact`. Most bridges declare replay unsupported. | Each battery must say `supported`, `unsupported`, or `projection-only` in its docs. Until then, saved replay cases must fail closed on unknown events. |

The presence of a gap is not a license to invent a private workaround. Each
gap above has been read and accepted; if your battery needs something not on
this list, that gap belongs in the table.

## What a battery is not

- A battery is not a framework blob. Each battery still depends on a
  specific slice of Tina core; no umbrella crate that re-exports everything.
- A battery is not a place to hide async runtimes. Tokio is allowed at the
  bridge worker, but the queue between the worker and the caller stays
  bounded and visible.
- A battery is not a place to invent new core semantics. New runtime
  primitives belong in `tina-runtime` with their own design and tests, not
  inside a battery crate.

When in doubt, write the smallest battery that still passes the checklist
above, then come back later to add ergonomics. Tina batteries should feel
boring.
