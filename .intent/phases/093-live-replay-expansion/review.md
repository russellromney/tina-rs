# 093 Hostile Review

## Verdict

Good only if it stays concrete.

The danger is building an observability platform or pretending live traces are
replay cases. The plan says no. Keep saying no.

## Required Guardrails

- Pick one edge. Do not support every protocol.
- App operations must be recorded explicitly. Do not infer them from trace.
- Unknown event kinds fail closed.
- Unsupported live facts are allowed, but they must block a "pass".
- Shrink only history ops.

## Review Focus

- Mismatch messages must tell the next action.
- Projection config must be visible.
- Config/topology/mailboxes must travel with the case.
- Any new fact must have a sim meaning or be explicitly unsupported.

## Good First Edge

If WebSocket is not merged yet, use HTTP/1 keepalive/body pressure or sharded
hot-key pressure. It is better to prove one boring real edge than to wait for
the perfect protocol specimen.
