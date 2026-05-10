//! Tokio: a `Counter` wrapped in `Arc<Mutex<u64>>`, three sequential
//! `reqwest::Client::post` calls. The state mutation and the webhook
//! POST are interleaved by `await` points; nothing back-pressures or
//! pages on overload.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{Report, WebhookServer};

const REQUESTS: usize = 3;

pub fn run() -> Report {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let webhook = WebhookServer::spawn();
    let url = webhook.url();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("reqwest client");
    let counter = Arc::new(Mutex::new(0u64));

    runtime.block_on(async move {
        for _ in 0..REQUESTS {
            let value = {
                let mut guard = counter.lock().expect("counter lock");
                *guard += 1;
                *guard
            };
            let body = value.to_string();
            let response = client
                .post(&url)
                .body(body)
                .send()
                .await
                .expect("tokio reqwest post");
            assert!(response.status().is_success(), "tokio webhook 200");
        }
    });

    // Give the server's writes a moment to land in the bodies vector.
    std::thread::sleep(Duration::from_millis(20));

    let bodies = webhook.snapshot();
    webhook.stop();
    Report { bodies }
}
