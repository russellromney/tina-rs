# 087 Hostile Review

## Verdict

Plan is good enough to implement.

Main risk: WebSocket can become a tiny unbounded broker by accident. Keep every
queue named and capped.

## Fixes Already Folded In

- Updated stale parallel-work note. Do not run broad WebSocket and gRPC
  `tina-http` work in the same files without coordination.
- Added HTTP/2 regression check when shared listener/connection code changes.

## Watch During Review

- Upgrade must not silently accept unsupported extensions.
- Fragmentation behavior must be one clear rule: reject, or bounded reassemble.
- Room specimen must show slow peer truth, not only happy broadcast.
- Close handshake must close the resource eventually.
- No hidden Tokio / async WebSocket crate.
