//! Real-path bridge e2e: the tokio bridge drives the **production**
//! `tina_rpc::Client` isolate against a **real** `tina-rpc` server over a real
//! loopback TCP socket.
//!
//! The other integration suite (`bridge.rs`) substitutes a `ClientStub` for the
//! `Client`, so the production client isolate is never exercised over the wire
//! there. This suite closes that gap: one `ThreadedRuntime` hosts both a full
//! server (Listener → Connection → Registry → SingleService) and the real
//! `Client`, and a `BridgeClient::call` awaits a byte-for-byte round trip:
//!
//! ```text
//! BridgeClient::call
//!   -> ClientMsg::Request (real Client isolate)
//!   -> encode frame -> TCP write -> server Connection -> Registry -> Service
//!   -> reply frame -> TCP read -> Client matches request_id
//!   -> ClientResultMsg -> bridge shim -> oneshot -> await returns Ok(bytes)
//! ```
//!
//! Self-contained (loopback only), so it runs by default — no `#[ignore]`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_rpc::{
    Client, ClientInit, ClientMsg, ClientRequest, ClientResultMsg, Connection, ConnectionInit,
    ConnectionMsg, EncodingError, Registry, RegistryMsg, RouterReply, ServiceCall, ServiceHandler,
    ServiceReply, SingleService,
};
use tina_rpc_tokio::{BridgeClient, BridgeError};
use tina_runtime::{
    DefaultThreadedMailboxFactory, ListenerId, TcpAcceptReply, TcpBindReply, TcpListenerCloseReply,
    ThreadedRuntime, tcp_accept, tcp_bind, tcp_close_listener,
};

/// `ping` echoes the raw payload; anything else is UnknownMethod.
struct EchoHandler;

impl ServiceHandler<SingleShard> for EchoHandler {
    fn dispatch(&mut self, call: ServiceCall) -> ServiceReply {
        match call.method.as_str() {
            "ping" => ServiceReply::Ok(call.payload),
            _ => ServiceReply::UnknownMethod,
        }
    }
}

#[derive(Debug, Clone)]
enum ListenerMsg {
    Start,
    Bound(TcpBindReply),
    Accepted(TcpAcceptReply),
    Closed(TcpListenerCloseReply),
}

struct Listener {
    bind_addr: SocketAddr,
    router: Address<RegistryMsg, RouterReply>,
    listener_id: Option<ListenerId>,
}

#[tina_runtime::isolate(
    message = ListenerMsg,
    spawn = ChildDefinition<Connection<SingleShard>>,
)]
impl Listener {
    fn handle(
        &mut self,
        msg: ListenerMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ListenerMsg::Start => tcp_bind(self.bind_addr).then(ListenerMsg::Bound),
            ListenerMsg::Bound(Ok((listener, _local))) => {
                self.listener_id = Some(listener);
                tcp_accept(listener).then(ListenerMsg::Accepted)
            }
            ListenerMsg::Accepted(Ok((stream, _peer))) => {
                let listener = self.listener_id.expect("listener set after bind");
                let connection =
                    Connection::<SingleShard>::new(ConnectionInit::new(stream, self.router));
                batch(vec![
                    spawn(
                        ChildDefinition::new(connection, 64)
                            .with_initial_message(ConnectionMsg::Begin),
                    ),
                    tcp_close_listener(listener).then(ListenerMsg::Closed),
                ])
            }
            ListenerMsg::Closed(Ok(())) => stop(),
            ListenerMsg::Bound(Err(_))
            | ListenerMsg::Accepted(Err(_))
            | ListenerMsg::Closed(Err(_)) => stop(),
        }
    }
}

#[tokio::test]
async fn bridge_drives_real_client_against_real_server_over_tcp() {
    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    // --- Server side: Registry -> SingleService, fronted by a Listener. ---
    let service = runtime
        .register_with_capacity::<_, Infallible>(SingleService::new(EchoHandler), 16)
        .expect("register service");
    let registry_state = Registry::<SingleShard>::builder()
        .service("echo", service)
        .build();
    let registry = runtime
        .register_with_capacity::<_, Infallible>(registry_state, 16)
        .expect("register registry");

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = runtime
        .register_with_capacity::<_, Infallible>(
            Listener {
                bind_addr,
                router: registry,
                listener_id: None,
            },
            8,
        )
        .expect("register listener");

    let bound = runtime
        .observe_next_bound()
        .expect("register listener bind observer");
    runtime
        .try_send(listener, ListenerMsg::Start)
        .expect("start listener");
    let server_addr = bound.wait(Duration::from_secs(3)).expect("listener bind");

    // --- Client side: the REAL Client isolate dialing the server. ---
    let client = runtime
        .register_with_capacity::<Client<SingleShard>, ClientResultMsg>(
            Client::new(ClientInit::<SingleShard>::connect(server_addr)),
            64,
        )
        .expect("register real client");
    // Begin dials the server and arms the read loop. Requests submitted before
    // Connected are queued by the client and flushed once the dial completes.
    runtime
        .try_send(client, ClientMsg::Begin)
        .expect("begin client");

    let bridge =
        BridgeClient::<SingleShard>::new(Arc::clone(&runtime), client, 16, 64).expect("bridge");

    // Drive one request through the real Client / bridge and await a real
    // reply. Raw request/decoder (no macro) so the assertion is byte-exact.
    let spawned_bridge = bridge.clone();
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::spawn(async move {
            spawned_bridge
                .call(
                    |corr, reply_to| {
                        Ok::<_, EncodingError>(ClientRequest {
                            service: "echo".into(),
                            method: "ping".into(),
                            payload: b"hello-real-path".to_vec(),
                            deadline: Duration::from_secs(5),
                            correlator: corr,
                            reply_to,
                        })
                    },
                    |bytes: &[u8]| Ok::<_, EncodingError>(bytes.to_vec()),
                )
                .await
        }),
    )
    .await
    .expect("spawned bridge call must not hang against a live server")
    .expect("spawned bridge task must not panic")
    .expect("real client round trip must return a reply");

    assert_eq!(
        reply,
        b"hello-real-path".to_vec(),
        "the real Client isolate must round-trip the payload through the real \
         server, not a stub"
    );

    // Also assert an unknown method surfaces the server's typed error through
    // the same real path. Bounded like the first call: a "connection alive but
    // no reply" regression must fail the test, not hang it forever.
    let unknown = tokio::time::timeout(
        Duration::from_secs(5),
        bridge.call(
            |corr, reply_to| {
                Ok::<_, EncodingError>(ClientRequest {
                    service: "echo".into(),
                    method: "nope".into(),
                    payload: Vec::new(),
                    deadline: Duration::from_secs(5),
                    correlator: corr,
                    reply_to,
                })
            },
            |bytes: &[u8]| Ok::<_, EncodingError>(bytes.to_vec()),
        ),
    )
    .await
    .expect("unknown-method call must not hang against a live server");
    assert!(
        matches!(unknown, Err(BridgeError::Server(_))),
        "unknown method must surface as a server error over the real path, got {unknown:?}",
    );

    // Tear the client connection down; the runtime Arc drops at scope end and
    // joins the worker thread.
    runtime
        .try_send(client, ClientMsg::Shutdown)
        .expect("shutdown client");
    drop(bridge);
}
