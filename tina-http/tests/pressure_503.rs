//! Wire-level overload behaviour.
//!
//! Fires many concurrent inbound TCP requests at the native server
//! and asserts every response is well-formed — `200`, `429 Too Many
//! Requests`, or `503 Service Unavailable`, never truncated or hung.
//! Exercises the multi-connection accept/dispatch path under load.
//!
//! Deterministic Full→429 on the wire is proven by
//! `typed_service_delivery::full_service_mailbox_returns_429` (and the
//! event twin) via a zero-capacity service mailbox. This file only
//! checks that concurrent bursts stay well-formed under load.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use common::HarnessConfig;

/// Fires `n` near-simultaneous TCP requests at the native server and
/// counts the response statuses.
#[test]
fn concurrent_burst_produces_well_formed_responses() {
    let harness = common::TestHarness::start_with_config(HarnessConfig {
        // Tight service mailbox so a burst has the best chance of
        // exercising any dispatch-side admission paths.
        service_mailbox_capacity: 1,
        ..Default::default()
    });
    let target = harness.addr;

    let success_count = Arc::new(AtomicUsize::new(0));
    let unavailable_count = Arc::new(AtomicUsize::new(0));
    let other_count = Arc::new(AtomicUsize::new(0));

    let total: usize = 8;
    let mut handles = Vec::with_capacity(total);
    for _ in 0..total {
        let success = Arc::clone(&success_count);
        let unavailable = Arc::clone(&unavailable_count);
        let other = Arc::clone(&other_count);
        handles.push(thread::spawn(move || {
            let Ok(mut stream) = TcpStream::connect_timeout(&target, Duration::from_secs(2)) else {
                other.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ =
                stream.write_all(b"GET /counter HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
            let _ = stream.flush();
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf);
            let text = String::from_utf8_lossy(&buf);
            if text.starts_with("HTTP/1.1 200") {
                success.fetch_add(1, Ordering::Relaxed);
            } else if text.starts_with("HTTP/1.1 429") || text.starts_with("HTTP/1.1 503") {
                unavailable.fetch_add(1, Ordering::Relaxed);
            } else {
                other.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for handle in handles {
        handle.join().expect("client thread");
    }

    let s = success_count.load(Ordering::Relaxed);
    let u = unavailable_count.load(Ordering::Relaxed);
    let o = other_count.load(Ordering::Relaxed);
    assert_eq!(
        s + u + o,
        total,
        "every request must produce a counted outcome"
    );
    assert_eq!(
        o, 0,
        "no truncated/timed-out responses: success={s} 503={u} other={o}"
    );
    // 429/503 may or may not appear depending on dispatch interleaving;
    // the key invariant is "no malformed or hung responses." Fragile
    // status counts are not pinned here.
    assert!(
        s + u == total,
        "expected only 200/429/503, got success={s} pressure={u}"
    );

    harness.shutdown();
}
