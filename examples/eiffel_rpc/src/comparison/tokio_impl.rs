//! Tokio reference: same wire protocol, unbounded `mpsc` queue, single
//! worker. Every request gets a Reply; nothing is rejected with Full.

use std::thread;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use tina_rpc::{
    Frame, FrameKind, FrameLimits, LENGTH_PREFIX_SIZE, decode_body, encode, parse_length_prefix,
};

use super::{ComparisonConfig, SideReport, drive_client};

pub(crate) fn run(config: ComparisonConfig) -> SideReport {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tokio listener");
        let addr = listener.local_addr().expect("tokio listener local addr");

        // Spawn the client in a non-tokio thread so it issues blocking
        // std::net I/O without needing to share the tokio runtime.
        let burst = config.burst;
        let client = thread::spawn(move || drive_client(addr, burst));

        let (stream, _peer_addr) = listener.accept().await.expect("tokio accept");
        serve(stream, config).await;

        client.join().expect("client thread")
    })
}

async fn serve(stream: tokio::net::TcpStream, config: ComparisonConfig) {
    let limits = FrameLimits::default();
    let (mut reader, mut writer) = stream.into_split();

    // Unbounded request queue + single worker. Every request is accepted
    // and eventually replied to. This is the "silent buffering" path
    // that Tina's framed first form refuses to implement.
    let (tx, mut rx) = mpsc::unbounded_channel::<Frame>();
    let writer_task = tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let reply = Frame::reply(req.request_id, req.service, req.method, req.payload);
            let bytes = encode(&reply, &FrameLimits::default()).expect("encode reply");
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut received = 0usize;
    while received < config.burst {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        loop {
            if buf.len() < LENGTH_PREFIX_SIZE {
                break;
            }
            let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
            prefix.copy_from_slice(&buf[..LENGTH_PREFIX_SIZE]);
            let body_len = match parse_length_prefix(prefix, &limits) {
                Ok(len) => len,
                Err(_) => return, // bad client; abandon
            };
            let total = LENGTH_PREFIX_SIZE + body_len;
            if buf.len() < total {
                break;
            }
            let body = buf[LENGTH_PREFIX_SIZE..total].to_vec();
            buf.drain(..total);
            let frame = match decode_body(&body) {
                Ok(f) => f,
                Err(_) => return,
            };
            if frame.kind != FrameKind::Request {
                return;
            }
            received += 1;
            // Always accept. Unbounded queue absorbs everything.
            let _ = tx.send(frame);
        }
    }
    drop(tx); // signal writer task to drain and exit
    let _ = writer_task.await;
}
