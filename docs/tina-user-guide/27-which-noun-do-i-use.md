# Which Noun Do I Use?

Start from the task, not the type list.

| Task | Use |
|---|---|
| limit work | `ConcurrencyLimit`, `KeyedLimit`, `SharedCapacityScope`, or a bounded mailbox |
| retry after `Full` | `FullHandling` when retry policy is the service contract |
| call another service | native `tina-http` HTTP/2/gRPC/WebSocket client helpers or a bridge crate |
| stream local bytes | `tina-codec` plus runtime file/Unix/TCP loops |
| own durable state | `DurableOutbox` / persistence helpers; restore before readiness |
| shut down | `DrainState`, `ServiceShutdownReport`, explicit close/cancel outcomes |
| capture a bug | `RunCapture::new("name").observer()` before the runtime starts |
| save/replay/shrink this bug | `save_bug`, `replay_bug`, `shrink_bug` |
| prove a hot path did not starve a cold path | `cold_work_made_progress`, `timer_kept_firing`, and fairness/load reports |
| control a session app | `WebSocketSessionMsg::AppControl(WebSocketSessionControl::...)` |
| wait for all calls | `CallJoinSet` |
| handle whichever call returns next | `CallSelectSet` |

