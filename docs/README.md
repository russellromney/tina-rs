# Tina Docs

Start here:

- [First Isolate](tina-user-guide/02-first-isolate.md) — the smallest complete
  program and the core handler/runtime loop
- [Tina User Guide](tina-user-guide/README.md)
- [Agent Quickstart](tina-user-guide/00-agent-quickstart.md) — a compact
  implementation checklist for coding agents and experienced contributors
- [Core And Batteries](tina-user-guide/23-core-and-batteries.md) — where the
  Tina core ends and the blessed batteries begin
- [Battery Authoring Checklist](tina-user-guide/24-battery-authoring.md) —
  what a battery (first- or third-party) owes the rest of the system
- [Extension Hooks](tina-user-guide/25-extension-hooks.md) — public seams a
  third-party crate uses to add codecs, policies, surfaces, sinks, and bridges
- [The Async Boundary](tina-user-guide/26-async-boundary.md) — native vs bridge
  vs unsupported for common Tokio ecosystem needs
- [Resource Owner Matrix](resource-owner-matrix.md) — who owns the
  close/drain/force/report path for each long-lived resource kind

The guide is practical and model-first. It explains the current public shape
without requiring readers to reverse-engineer the R&D specimen corpus.

The docs explain the current user-facing shape for isolates, effects, runtime
calls, request/reply continuations, service topology, boundedness, supervision,
simulation, and runtime-owned I/O.
