# Tina User Guide

Grug guide for using Tina.

Tina is small machine:

- isolate owns state
- isolate handles one message at a time
- handler returns an effect
- runtime performs effects
- every queue should have a budget
- overload should say `Full`, `Closed`, or `Timeout`
- same isolate should run live and in simulation

This guide is for people porting small Tokio-shaped libraries into Tina and
writing comparison examples for Eiffel.

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
10. [Ergonomics Notes](10-ergonomics-notes.md)
11. [Ergonomics Checklist](11-ergonomics-checklist.md)

For runnable specimens, see repo-root `examples/`.

The docs are intentionally plain. When in doubt, prefer a boring message enum,
a boring state struct, and a boring handler that returns one explicit effect.
