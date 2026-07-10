# Tina User Guide

Guide for building services with Tina's current public API.

Tina is small machine:

- isolate owns state
- isolate handles one message at a time
- handler returns an effect
- runtime performs effects
- every queue should have a budget
- overload should say `Full`, `Closed`, or `Timeout`
- same isolate should run live and in simulation

The rule: user docs teach the Tina model. The repository's specimen examples
are an R&D corpus used to test the model and discover API problems; they are
not yet a curated tutorial.

Read in order if new:

1. [Mental Model](01-mental-model.md)
2. [First Isolate](02-first-isolate.md)
3. [Effects And Runtime Calls](03-effects-and-runtime-calls.md)
4. [Request Reply](04-request-reply.md)
5. [TCP Services](05-tcp-services.md)
6. [Boundedness And Overload](06-boundedness-and-overload.md)
7. [Supervision](07-supervision.md)
8. [Simulation And DST](08-simulation-and-dst.md)
9. [Tokio To Tina Porting](09-tokio-to-tina-porting.md)
10. [Service Patterns](10-service-patterns.md)
11. [Ergonomics Checklist](11-ergonomics-checklist.md)
12. [I/O Model](12-io-model.md)
13. [Outcome Glossary](13-outcome-glossary.md)
14. [Lifecycle And Shutdown](14-lifecycle-and-shutdown.md)
15. [Service Client Worked Example](15-service-client-worked-example.md)
16. [Continuation And Pipeline Patterns](16-continuation-and-pipeline-patterns.md)
17. [Pressure Report Convention](17-pressure-report-convention.md)
18. [Bridge Crates](18-bridge-crates.md)
19. [Tracing](19-tracing.md)
20. [Native WebSocket Server And Client](20-native-websocket-server.md)
21. [Compile-Time Safety Rails](21-compile-time-safety-rails.md)
22. [HTTP/HTTP2/gRPC Protocol Facts](22-http-http2-grpc.md)
23. [Core And Batteries](23-core-and-batteries.md)
24. [Battery Authoring Checklist](24-battery-authoring.md)
25. [Extension Hooks](25-extension-hooks.md)
26. [The Async Boundary](26-async-boundary.md)
27. [Which Noun Do I Use?](27-which-noun-do-i-use.md)
28. [Outbound Clients: Endpoint → Policy → Manager](28-outbound-clients.md)
29. [Continuation Flows](29-continuation-flows.md)
30. [Bridge Author Kit](30-bridge-author-kit.md)

[Agent Quickstart](00-agent-quickstart.md) is a compact checklist for coding
agents and contributors who already understand the model.

The reading order separates **learn core** (pages 1–17, 19, 21) from
**choose batteries** (pages 18, 20, 22, 25, 28, plus 23–24 which explain the
boundary itself). New users should not need to read battery docs to
understand Tina core.

For the smallest runnable program, use
[`tina-runtime/examples/hello_world.rs`](../../tina-runtime/examples/hello_world.rs).
When in doubt, prefer a plain message enum, a small state struct, and a handler
that returns one explicit effect.

## Runtime Questions

If someone with a runtime/substrate brain reads Tina, they will ask:

- Where does I/O really happen?
- Which queues are bounded?
- What wakes the machine?
- What happens on close while work is pending?
- What is live behavior, and what is simulation?
- What perf claim is proved, and what is only possible later?

The short answer: Tina owns semantics and resource truth; Betelgeuse is the
canonical portable live I/O substrate; `tina-sim` is the deterministic oracle.
Linux and macOS use their Betelgeuse platform backends. Named bounded fallback
lanes remain for blocking DNS/process work and storage metadata operations not
provided by the substrate.
