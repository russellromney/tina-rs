# Phase 052: Tina Framed Calls First Form

## Goal

Build the smallest Tina-native framed request/reply probe.

Not gRPC. Not service mesh. Not schema empire. Not a general RPC framework.

052 answers:

> Can Tina model bounded request/reply over a byte stream, with timeouts and
> visible overload?

Near-grug:

> frame bytes. route method. call isolate. reply frame. timeout if sad.

## Baseline

Already exists:

- typed isolate addresses;
- isolate calls with timeout;
- cross-shard calls;
- runtime-owned TCP;
- trace;
- supervision;
- deterministic simulation.

Compromise:

- use `serde` for payload traits;
- use JSON as the first encoding because it is boring and debuggable;
- use boring length-delimited framing;
- use off-the-shelf codec/framing/building-block crates where they do not own
  I/O or scheduling;
- use boring Rust collections inside isolates;
- Tina owns connection, request ids, in-flight capacity, timeout, pressure, and
  shutdown.

## Non-Goals

- No gRPC.
- No general RPC framework.
- No protobuf requirement.
- No HTTP/2.
- No remoting/clustering membership yet.
- No exactly-once claim.
- No transparent distributed object system.
- No hidden unbounded in-flight map.
- No public wire compatibility promise. First form may change.
- No authentication, encryption, or authorization in first form. Put it behind
  local loopback/dev use or TLS/bridge layers until a later security phase.
- No cancellation frame. Server completes; client discards late reply.

## Rules

- Every request has a request id.
- Every request has a timeout.
- In-flight requests are bounded.
- Server-reported error replies are wire-visible: `full`,
  `unknown_service`, `unknown_method`, `decode`, `protocol`, `internal`.
  Client-observed conditions (`timeout`, `connection_closed`) are not
  wire frames; see the wire-error invariant below.
- Payload encoding is pluggable, but first form can pick one boring default.
- Trace links network frame to isolate call where practical.
- A bad peer cannot force unbounded memory growth.
- Max frame size, max in-flight requests, and idle timeout are explicit config.
- Wire errors are server-reported. Client-observed conditions (`timeout`,
  `connection_closed`) are surfaced locally and never appear as wire frames.
- If a crate wants to own sockets, block on `Read`/`Write`, spawn tasks, or hide
  queues, do not use it here.

## Rocks

1. **Frame Format**

   First form is deliberately boring and unstable:

   - length prefix;
   - version;
   - request id;
   - kind: request/reply/error;
   - service name;
   - method name;
   - payload bytes;
   - error code (server-reported only): `full`, `unknown_service`, `unknown_method`, `decode`, `protocol`, `internal`.

   Version field required. Max frame size always.

   Decode order: read length prefix, check against `max_frame_size`, then
   allocate and read body. Oversize is rejected before allocation.

2. **Connection Isolate**

   Requirements:

   - read frames from TCP;
   - decode frame;
   - route request to service isolate;
   - write reply/error frame;
   - track bounded in-flight requests;
   - close bad peers;
   - close idle connections after configured `idle_timeout`, visibly;
   - shutdown visibly.

3. **Service Registry**

   Small registry:

   - service name maps to isolate address;
   - connection isolate forwards request via existing isolate call with
     timeout;
   - unknown service gives error reply;
   - unknown method gives error reply;
   - service full maps to full reply (emergent from isolate call full);
   - service timeout maps to `internal` on the wire (a wire `timeout`
     frame would violate the wire-error invariant; the wire-error code
     vocabulary is `full`, `unknown_service`, `unknown_method`,
     `decode`, `protocol`, `internal` — no `timeout`). The
     client-observed timeout it produces locally still elapses on the
     client deadline.

4. **Client Stub First Form**

   Requirements:

   - connect;
   - send request;
   - wait for matching reply;
   - timeout;
   - bounded in-flight map;
   - when client in-flight map is full, return `Full` immediately. No
     queuing, no blocking;
   - out-of-order replies work;
   - connection close fails pending calls visibly.
   - idle timeout closes dead connections visibly.

5. **Encoding Adapter**

   Requirements:

   - trait for encode/decode;
   - first implementation: JSON;
   - postcard/bincode may be later adapters;
   - decode errors become typed RPC errors;
   - max payload size enforced.

6. **Simulation And Replay**

   Required scenarios:

   - reply before timeout;
   - timeout;
   - full service;
   - closed connection;
   - decode error;
   - out-of-order replies;
   - saved seed for at least one reordered history.

7. **Specimen RPC Comparison**

   Add comparison:

   - Tina framed calls first form;
   - Tokio framed reference;
   - same method behavior;
   - same report format;
   - overload/in-flight limit scenario.

## Required Proof

- Client calls server over real TCP and gets reply.
- Full, decode error, unknown service, and unknown method are wire-visible
  as server-reported error frames. Closed and timeout are
  client-observed conditions and surface locally (peer disconnect, local
  deadline elapses); they never appear as wire frames per the wire-error
  invariant in the Rules section.
- In-flight limit rejects visibly.
- Max frame size, max in-flight, and idle timeout are tested.
- Peer-close / half-open behavior fails pending calls visibly.
- Out-of-order replies are matched correctly.
- Simulator replay covers at least one reordered or timeout history.
- Docs say this is Tina framed calls first form, not gRPC and not a general RPC
  framework.

## Done Means

- Tina has a native framed-call seed that matches its model.
- Future remoting/clustering has a wire envelope to grow from.
- Users can experiment with simple service-to-service calls without Tokio owning
  the protocol.
