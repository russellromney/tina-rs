# Native WebSocket Room Specimen

Small Tina-native WebSocket room shape over `tina-http`'s HTTP/1.1
upgrade path.

The HTTP listener owns TCP accept and request parsing. A `GET /room`
upgrade returns `HttpResponse::websocket(...)`, after which the
connection isolate becomes the WebSocket session owner. The room app
receives visible `Open`, `Text`, `Binary`, `Ping`, `Pong`, `Close`,
`Pressure`, and `Closed` messages and returns bounded outbound
commands.

This specimen keeps fanout deliberately tiny. Its smoke test proves
the WebSocket path and separately proves that room broadcast pressure
reports a distinct `Full` result instead of hiding an unbounded queue.
