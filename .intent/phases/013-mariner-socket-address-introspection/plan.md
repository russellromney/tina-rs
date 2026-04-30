# 013 Mariner Socket Address Introspection Plan

Session:

- A

## Goal

Close the remaining address-honesty gap in the TCP-first call family by
supporting real `local_addr` and `peer_addr` on the Betelgeuse-backed runtime.

## Decisions

- This slice is about truthful socket address reporting, not about timers,
  multi-connection re-arm, or simulator work.
- The runtime may temporarily depend on a pinned Betelgeuse fork/commit if the
  required address-introspection surface is not yet available upstream.
- `TcpBind { addr: 127.0.0.1:0 }` should become supported again only when the
  runtime can report the actual bound address honestly.
- `CallResult::TcpAccepted` should include `peer_addr` only when it comes from
  the backend honestly, not from a placeholder or secondary probe.

## Steps

1. Inspect the current Betelgeuse surface for:
   - listener local address access
   - accepted stream peer address access
2. If missing, add the smallest honest backend surface needed.
3. Wire the new surface into `tina-runtime-current`:
   - support `:0` binds honestly
   - restore `peer_addr` on accepted streams
4. Update tests and example:
   - `tcp_echo.rs` returns to `127.0.0.1:0`
   - prove the returned address is the real connect target
5. Update docs and closeout artifacts.

## Verification

- Focused tests for:
  - successful `:0` bind returns non-zero `local_addr.port()`
  - accepted stream reports a real `peer_addr`
- TCP echo integration test on `127.0.0.1:0`
- Runnable `tcp_echo` example still works
- `make verify`

## Pause Gates

Stop and ask for human input only if one of these happens:

- the required Betelgeuse change turns out to be materially larger than a small
  address-introspection surface
- restoring truthful address reporting would force `tina` API changes rather
  than runtime-crate changes
- the runtime can only obtain addresses by a side probe or TOCTOU workaround
- the upstream shape introduces a different architectural direction than
  caller-driven explicit stepping

## Traps

- Do not reintroduce TOCTOU address reconstruction.
- Do not silently keep `:0` rejected while claiming the gap is closed.
- Do not add backend-specific concepts to the `tina` trait crate.
- Do not bundle sleep/timer work into this slice.
