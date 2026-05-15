use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tina::prelude::*;
use tina_reqwest_bridge::{
    ReqwestAddress, ReqwestCallOutcome, ReqwestConfig, ReqwestRequest, ReqwestWorker, send_request,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, ThreadedRuntime};
use tina_sqlite_bridge::{
    SqliteAddress, SqliteCloser, SqliteConfig, SqliteRows, SqliteValue, SqliteWorker, execute_call,
    query_call,
};

#[derive(Debug, Clone)]
pub struct Item {
    pub id: u64,
    pub name: String,
}

#[derive(Debug)]
pub enum ApiMsg {
    CreateItem {
        name: String,
    },
    GetItem {
        id: u64,
    },
    Health,
    Readiness,
    FireWebhook {
        item_id: u64,
        item_name: String,
    },
    DbCreated {
        slot: DeferredReply<ApiReply>,
        item_id: u64,
        item_name: String,
    },
    DbItemFound {
        slot: DeferredReply<ApiReply>,
        item: Option<Item>,
    },
    DbPing {
        slot: DeferredReply<ApiReply>,
        ok: bool,
    },
    WebhookDone(ReqwestCallOutcome),
    SelfError {
        slot: DeferredReply<ApiReply>,
        msg: String,
    },
}

#[derive(Debug, Clone)]
pub enum ApiReply {
    HealthOk,
    Ready,
    NotReady(String),
    ItemCreated(Item),
    ItemFound(Item),
    ItemNotFound,
    DbError(String),
    Error(String),
}

use ApiReply::*;

struct Api {
    db: SqliteAddress,
    webhook_url: String,
    webhook: ReqwestAddress,
    next_id: u64,
    db_timeout: Duration,
    webhook_timeout: Duration,
    me: Address<ApiMsg, ApiReply>,
}

#[tina_runtime::isolate(message = ApiMsg, reply = ApiReply, send = Outbound<ApiMsg>)]
impl Api {
    fn handle(
        &mut self,
        msg: ApiMsg,
        ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            ApiMsg::Health => reply(HealthOk),

            ApiMsg::Readiness => {
                let Ok(slot) = ctx.take_reply_slot() else {
                    return reply(NotReady("no caller".into()));
                };
                query_call(self.db, "SELECT 1", vec![], 1, self.db_timeout).reply(move |outcome| {
                    ApiMsg::DbPing {
                        slot,
                        ok: matches!(outcome, CallOutcome::Replied(Ok(rows)) if !rows.is_empty()),
                    }
                })
            }

            ApiMsg::DbPing { slot, ok } => {
                if ok {
                    reply_to(slot, Ready)
                } else {
                    reply_to(slot, NotReady("db unreachable".into()))
                }
            }

            ApiMsg::CreateItem { name } => {
                let Ok(slot) = ctx.take_reply_slot() else {
                    return reply(Error("no caller".into()));
                };
                let id = self.next_id;
                self.next_id += 1;
                execute_call(
                    self.db,
                    "INSERT INTO items (id, name) VALUES (?, ?)",
                    vec![
                        SqliteValue::Integer(id as i64),
                        SqliteValue::Text(name.clone()),
                    ],
                    self.db_timeout,
                )
                .reply(move |outcome| match outcome {
                    CallOutcome::Replied(Ok(_rows)) => ApiMsg::DbCreated {
                        slot,
                        item_id: id,
                        item_name: name,
                    },
                    CallOutcome::Replied(Err(e)) => ApiMsg::SelfError {
                        slot,
                        msg: format!("{e}"),
                    },
                    other => ApiMsg::SelfError {
                        slot,
                        msg: format!("{other:?}"),
                    },
                })
            }

            ApiMsg::DbCreated {
                slot,
                item_id,
                item_name,
            } => {
                let item = Item {
                    id: item_id,
                    name: item_name.clone(),
                };
                batch([
                    reply_to(slot, ItemCreated(item)),
                    send(self.me, ApiMsg::FireWebhook { item_id, item_name }),
                ])
            }

            ApiMsg::GetItem { id } => {
                let Ok(slot) = ctx.take_reply_slot() else {
                    return reply(Error("no caller".into()));
                };
                query_call(
                    self.db,
                    "SELECT id, name FROM items WHERE id = ?",
                    vec![SqliteValue::Integer(id as i64)],
                    1,
                    self.db_timeout,
                )
                .reply(move |outcome| {
                    let item = match outcome {
                        CallOutcome::Replied(Ok(SqliteRows { rows, .. })) if !rows.is_empty() => {
                            let row = &rows[0];
                            let item_id = row_value(row, 0).unwrap_or(0);
                            let item_name = row_string(row, 1).unwrap_or_default();
                            Some(Item {
                                id: item_id,
                                name: item_name,
                            })
                        }
                        _ => None,
                    };
                    ApiMsg::DbItemFound { slot, item }
                })
            }

            ApiMsg::DbItemFound { slot, item } => match item {
                Some(item) => reply_to(slot, ItemFound(item)),
                None => reply_to(slot, ItemNotFound),
            },

            ApiMsg::FireWebhook { item_id, item_name } => {
                let body = format!(
                    r#"{{"event":"item_created","id":{},"name":"{}"}}"#,
                    item_id, item_name
                );
                let request = ReqwestRequest::post(self.webhook_url.clone(), body.into_bytes());
                let mut headers = http::HeaderMap::new();
                headers.insert(
                    http::header::CONTENT_TYPE,
                    "application/json".parse().unwrap(),
                );
                let request = ReqwestRequest { headers, ..request };
                send_request(self.webhook, request, self.webhook_timeout).reply(ApiMsg::WebhookDone)
            }

            ApiMsg::WebhookDone(outcome) => {
                match outcome {
                    CallOutcome::Replied(Ok(_resp)) => {}
                    other => {
                        eprintln!("system_mini_saas_api: webhook call failed: {other:?}");
                    }
                }
                noop()
            }

            ApiMsg::SelfError { slot, msg } => reply_to(slot, DbError(msg)),
        }
    }
}

fn row_value(row: &[SqliteValue], idx: usize) -> Option<u64> {
    match row.get(idx)? {
        SqliteValue::Integer(i) => Some(*i as u64),
        _ => None,
    }
}

fn row_string(row: &[SqliteValue], idx: usize) -> Option<String> {
    match row.get(idx)? {
        SqliteValue::Text(s) => Some(s.clone()),
        SqliteValue::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

pub struct SystemHandle {
    runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    sqlite_closer: SqliteCloser,
    reqwest_closer: tina_reqwest_bridge::ReqwestCloser,
    server_addr: SocketAddr,
    tokio_rt: tokio::runtime::Runtime,
    db_path: PathBuf,
}

impl SystemHandle {
    pub fn server_addr(&self) -> SocketAddr {
        self.server_addr
    }

    pub fn shutdown(self) -> anyhow::Result<()> {
        drop(self.tokio_rt);
        self.sqlite_closer.close();
        self.reqwest_closer.close();
        match Arc::try_unwrap(self.runtime) {
            Ok(rt) => {
                rt.shutdown()?;
            }
            Err(_) => anyhow::bail!("runtime still has outstanding references"),
        }
        let _ = std::fs::remove_file(&self.db_path);
        Ok(())
    }
}

pub fn start(
    listen_addr: SocketAddr,
    db_path: PathBuf,
    webhook_url: &str,
) -> anyhow::Result<SystemHandle> {
    let conn = rusqlite::Connection::open(&db_path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS items (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL
        );",
    )?;
    drop(conn);

    let runtime = Arc::new(ThreadedRuntime::new(
        SingleShard,
        DefaultThreadedMailboxFactory,
    ));

    let sqlite_cfg = SqliteConfig::path(&db_path)
        .with_default_timeout(Duration::from_secs(5))
        .with_busy_timeout(Duration::from_secs(1))
        .with_pragma("journal_mode = WAL")
        .with_poll_interval(Duration::from_millis(1));
    let sqlite_bridge = SqliteWorker::<SingleShard>::install(&runtime, sqlite_cfg)
        .context("install sqlite bridge")?;

    let reqwest_cfg = ReqwestConfig::default();
    let reqwest_worker = ReqwestWorker::<SingleShard>::install(&runtime, reqwest_cfg)
        .context("install reqwest bridge")?;

    let webhook_owned = webhook_url.to_string();
    let api_addr = runtime
        .register_with_capacity_using::<Api, ApiMsg, _>(16, move |self_addr| Api {
            db: sqlite_bridge.address,
            webhook_url: webhook_owned,
            webhook: reqwest_worker.address,
            next_id: 1,
            db_timeout: Duration::from_secs(5),
            webhook_timeout: Duration::from_secs(5),
            me: self_addr,
        })
        .map_err(|e| anyhow::anyhow!("register api: {e:?}"))?;

    let sqlite_closer = sqlite_bridge.closer;
    let reqwest_closer = reqwest_worker.closer;
    let runtime_clone = Arc::clone(&runtime);
    let db_path_clone = db_path.clone();

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let _handle = tokio_rt.spawn(hyper_server(listen_addr, runtime_clone, api_addr, addr_tx));
    let server_addr = addr_rx
        .recv_timeout(Duration::from_secs(3))
        .context("hyper server did not bind in time")?;

    Ok(SystemHandle {
        runtime,
        sqlite_closer,
        reqwest_closer,
        server_addr,
        tokio_rt,
        db_path: db_path_clone,
    })
}

async fn hyper_server(
    listen_addr: SocketAddr,
    runtime: Arc<ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>>,
    api: Address<ApiMsg, ApiReply>,
    addr_tx: std::sync::mpsc::Sender<SocketAddr>,
) {
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;

    let listener = tokio::net::TcpListener::bind(listen_addr).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let _ = addr_tx.send(bound);
    println!("system_mini_saas_api HTTP listening on {}", bound);

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => break,
        };
        let rt = Arc::clone(&runtime);
        let api = api;

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: hyper::Request<Incoming>| {
                let rt = Arc::clone(&rt);
                let api = api;
                async move { handle_http(&rt, api, req).await }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await
            {
                eprintln!("connection error: {e}");
            }
        });
    }
}

async fn handle_http(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    api: Address<ApiMsg, ApiReply>,
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, hyper::Error> {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};

    let (parts, body) = req.into_parts();
    let path = parts.uri.path().to_string();
    let method = parts.method;
    let body_bytes = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let timeout = Duration::from_secs(30);

    let reply = match (method.as_str(), path.as_str()) {
        ("GET", "/health") => call_tina(runtime, api, ApiMsg::Health, timeout),
        ("GET", "/readiness") => call_tina(runtime, api, ApiMsg::Readiness, timeout),
        ("POST", "/items") => {
            let raw = String::from_utf8_lossy(&body_bytes);
            let name = raw.trim().to_string();
            let name = if name.is_empty() {
                "unnamed".into()
            } else {
                name
            };
            call_tina(runtime, api, ApiMsg::CreateItem { name }, timeout)
        }
        ("GET", path) if path.starts_with("/items/") => {
            let id_str = &path["/items/".len()..];
            let id: u64 = id_str.parse().unwrap_or(0);
            call_tina(runtime, api, ApiMsg::GetItem { id }, timeout)
        }
        _ => Error("not found".into()),
    };

    let (status, body_str) = match &reply {
        HealthOk => (200, r#"{"status":"ok"}"#.to_string()),
        Ready => (200, r#"{"status":"ready"}"#.to_string()),
        NotReady(msg) => (
            503,
            format!(r#"{{"status":"not_ready","detail":"{}"}}"#, msg),
        ),
        ItemCreated(item) => (
            201,
            format!(r#"{{"id":{},"name":"{}"}}"#, item.id, item.name),
        ),
        ItemFound(item) => (
            200,
            format!(r#"{{"id":{},"name":"{}"}}"#, item.id, item.name),
        ),
        ItemNotFound => (404, r#"{"error":"not found"}"#.to_string()),
        DbError(msg) => (500, format!(r#"{{"error":"db: {}"}}"#, msg)),
        Error(msg) => (500, format!(r#"{{"error":"{}"}}"#, msg)),
    };

    Ok(hyper::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body_str)))
        .unwrap())
}

fn call_tina(
    runtime: &ThreadedRuntime<SingleShard, DefaultThreadedMailboxFactory>,
    api: Address<ApiMsg, ApiReply>,
    msg: ApiMsg,
    timeout: Duration,
) -> ApiReply {
    match runtime.call_blocking(api, msg, timeout) {
        Ok(CallOutcome::Replied(reply)) => reply,
        Ok(CallOutcome::Timeout) => Error("timeout".into()),
        Ok(_) => Error("call failed".into()),
        Err(e) => Error(format!("{e}")),
    }
}

pub fn main() -> anyhow::Result<()> {
    let db_path = PathBuf::from("/tmp/system_mini_saas_api.sqlite");
    let webhook_url = "http://127.0.0.1:9999/webhook";
    let listen_addr: SocketAddr = "127.0.0.1:8081".parse()?;

    let handle = start(listen_addr, db_path, webhook_url)?;
    println!("system_mini_saas_api running on {}", handle.server_addr());
    std::thread::park();
    handle.shutdown()?;
    Ok(())
}
