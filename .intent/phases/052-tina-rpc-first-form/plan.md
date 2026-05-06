# Phase 052: Tina RPC First Form

## Goal

Build the smallest Tina-native RPC.

Not gRPC. Not service mesh. Not schema empire.

052 answers:

> Can Tina talk Tina over TCP with bounded request/reply, timeouts, and visible
> overload?

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
- use JSON, postcard, or bincode as first encoding;
- use length-delimited frames;
- use boring Rust collections inside isolates;
- Tina owns connection, request ids, in-flight capacity, timeout, pressure, and
  shutdown.

## Non-Goals

- No gRPC.
- No protobuf requirement.
- No HTTP/2.
- No remoting/clustering membership yet.
- No exactly-once claim.
- No transparent distributed object system.
- No hidden unbounded in-flight map.

## Rules

- Every request has a request id.
- Every request has a timeout.
- In-flight requests are bounded.
- Full/closed/timeout/error replies are wire-visible.
- Payload encoding is pluggable, but first form can pick one boring default.
- Trace links network frame to isolate call where practical.
- A bad peer cannot force unbounded memory growth.

## Rocks

1. **Frame Format**

   First form:

   - length prefix;
   - request id;
   - kind: request/reply/error;
   - service name;
   - method name;
   - payload bytes;
   - error code for full/closed/timeout/decode/protocol.

   Version field if cheap. Max frame size always.

2. **Connection Isolate**

   Requirements:

   - read frames from TCP;
   - decode frame;
   - route request to service isolate;
   - write reply/error frame;
   - track bounded in-flight requests;
   - close bad peers;
   - shutdown visibly.

3. **Service Registry**

   Small registry:

   - service name maps to address/handler;
   - unknown service gives error reply;
   - unknown method gives error reply;
   - service full maps to full reply;
   - service timeout maps to timeout reply.

4. **Client Stub First Form**

   Requirements:

   - connect;
   - send request;
   - wait for matching reply;
   - timeout;
   - bounded in-flight map;
   - out-of-order replies work;
   - connection close fails pending calls visibly.

5. **Encoding Adapter**

   Requirements:

   - trait for encode/decode;
   - first implementation: JSON or postcard/bincode;
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

7. **Eiffel RPC Comparison**

   Add comparison:

   - Tina RPC first form;
   - Tokio framed reference;
   - same method behavior;
   - same report format;
   - overload/in-flight limit scenario.

## Required Proof

- Client calls server over real TCP and gets reply.
- Full, closed, timeout, decode error, and unknown method are wire-visible.
- In-flight limit rejects visibly.
- Out-of-order replies are matched correctly.
- Simulator replay covers at least one reordered or timeout history.
- Docs say this is Tina RPC first form, not gRPC.

## Done Means

- Tina has a native RPC seed that matches its model.
- Future remoting/clustering has a wire envelope to grow from.
- Users can build simple service-to-service calls without Tokio owning the
  protocol.
