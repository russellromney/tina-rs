use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[tokio::test]
async fn smoke() {
    let webhook_hit = Arc::new(AtomicBool::new(false));
    let webhook_body = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let webhook_hit2 = webhook_hit.clone();
    let webhook_body2 = webhook_body.clone();

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let webhook_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let webhook_addr = webhook_listener.local_addr().unwrap();
    let webhook_url = format!("http://{webhook_addr}/webhook");

    tokio::spawn(async move {
        use bytes::Bytes;
        use http_body_util::{BodyExt, Full};
        use hyper::body::Incoming;
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;

        loop {
            tokio::select! {
                result = webhook_listener.accept() => {
                    let (stream, _) = result.unwrap();
                    let hit = webhook_hit2.clone();
                    let body_store = webhook_body2.clone();
                    tokio::spawn(async move {
                        let io = TokioIo::new(stream);
                        let svc = service_fn(move |req: hyper::Request<Incoming>| {
                            let hit = hit.clone();
                            let body_store = body_store.clone();
                            async move {
                                let (parts, body) = req.into_parts();
                                if parts.uri.path() == "/webhook" {
                                    let bytes = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                                    *body_store.lock().unwrap() = bytes.to_vec();
                                    hit.store(true, Ordering::SeqCst);
                                }
                                let resp = hyper::Response::builder()
                                    .status(200)
                                    .body(Full::new(Bytes::from("ok"))).unwrap();
                                Ok::<_, hyper::Error>(resp)
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(io, svc).await;
                    });
                }
                _ = &mut shutdown_rx => break,
            }
        }
    });

    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("test.sqlite");

    let listen_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let handle = system_mini_saas_api::start(listen_addr, db_path.clone(), &webhook_url).unwrap();
    let server_addr = handle.server_addr();

    let client = reqwest::Client::new();

    // 1. Health check
    let resp = client
        .get(format!("http://{server_addr}/health"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");

    // 2. Readiness
    let resp = client
        .get(format!("http://{server_addr}/readiness"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 3. Create item
    let resp = client
        .post(format!("http://{server_addr}/items"))
        .body("test-item-1")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let item_id = body["id"].as_u64().unwrap();
    assert_eq!(body["name"], "test-item-1");

    // 4. Read item back
    let resp = client
        .get(format!("http://{server_addr}/items/{item_id}"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"].as_u64().unwrap(), item_id);
    assert_eq!(body["name"], "test-item-1");

    // 5. Read missing item
    let resp = client
        .get(format!("http://{server_addr}/items/99999"))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Give webhook time to fire (it's async after reply)
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        webhook_hit.load(Ordering::SeqCst),
        "webhook was not received"
    );
    let wb_body = webhook_body.lock().unwrap();
    let wb_str = String::from_utf8_lossy(&wb_body);
    assert!(
        wb_str.contains("item_created"),
        "webhook body unexpected: {wb_str}"
    );

    // 6. Shut down cleanly
    let _ = shutdown_tx.send(());
    tokio::task::spawn_blocking(move || handle.shutdown().unwrap())
        .await
        .unwrap();
}
