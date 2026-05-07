# TCP Services

TCP in Tina is runtime-owned.

The isolate does not hold `TcpListener` or `TcpStream`. It holds runtime IDs:

```rust
use tina_runtime::{ListenerId, StreamId};
```

Typical server shape:

```text
listener isolate binds
listener accepts stream
listener spawns connection isolate
connection isolate reads
connection isolate writes
connection isolate closes
```

## Listener

```rust
use std::net::SocketAddr;
use tina::prelude::*;
use tina_runtime::{tcp_accept, tcp_bind, CallError, ListenerId, StreamId};

#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound(Result<(ListenerId, SocketAddr), CallError>),
    Accepted(Result<(StreamId, SocketAddr), CallError>),
}

struct Listener {
    bind_addr: SocketAddr,
    listener: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    spawn = ChildDefinition<Connection>,
    shard = AppShard
)]
impl Listener {
    fn handle(&mut self, msg: ListenerMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => tcp_bind(self.bind_addr).reply(ListenerMsg::Bound),
            ListenerMsg::Bound(Ok((listener, _addr))) => {
                self.listener = Some(listener);
                tcp_accept(listener).reply(ListenerMsg::Accepted)
            }
            ListenerMsg::Bound(Err(_)) => stop(),
            ListenerMsg::Accepted(Ok((stream, _peer))) => {
                let listener = self.listener.expect("bound before accept");
                batch(vec![
                    spawn(ChildDefinition::new(Connection { stream }, 16)),
                    tcp_accept(listener).reply(ListenerMsg::Accepted),
                ])
            }
            ListenerMsg::Accepted(Err(_)) => stop(),
        }
    }
}
```

## Connection

```rust
use tina_runtime::{tcp_close_stream, tcp_read, tcp_write};

#[derive(Debug, Clone)]
enum ConnMsg {
    Begin,
    Read(Result<Vec<u8>, CallError>),
    Wrote(Result<usize, CallError>),
    Closed(Result<(), CallError>),
}

struct Connection {
    stream: StreamId,
}

#[tina_runtime::isolate(message = ConnMsg, shard = AppShard)]
impl Connection {
    fn handle(&mut self, msg: ConnMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            ConnMsg::Begin => tcp_read(self.stream, 4096).reply(ConnMsg::Read),
            ConnMsg::Read(Ok(bytes)) => tcp_write(self.stream, bytes).reply(ConnMsg::Wrote),
            ConnMsg::Read(Err(_)) => stop(),
            ConnMsg::Wrote(_) => tcp_close_stream(self.stream).reply(ConnMsg::Closed),
            ConnMsg::Closed(_) => stop(),
        }
    }
}
```

## Common Pain

TCP ports expose Tina ergonomics fast.

Watch for:

- connection state enum getting large
- `Result<_, CallError>` repeated everywhere
- write loops needing careful chunk state
- accept loop plus spawn requiring `batch`
- shutdown and close paths feeling verbose

Do not hide pain too early. First write the honest state machine. Then extract
helpers only when the repeated shape is real.
