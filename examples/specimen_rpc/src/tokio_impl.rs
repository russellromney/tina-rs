//! Tokio reference: same wire frame, unbounded `mpsc` queue, single
//! worker. Every request is accepted into the queue and eventually
//! replied to. The wire never carries an `Error(Full)` — overload
//! is invisible at the protocol level because the queue silently
//! grows.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use tina_rpc::{
    Frame, FrameError, FrameKind, FrameLimits, LENGTH_PREFIX_SIZE, decode_body, encode,
    parse_length_prefix,
};

use crate::{Report, RunConfig};

pub fn run(config: RunConfig) -> anyhow::Result<Report> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    runtime.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let burst = config.burst;

        let client = tokio::spawn(async move { drive_client(addr, burst).await });
        let (stream, _peer) = listener.accept().await?;
        serve(stream, burst).await?;
        client.await?
    })
}

/// Server: read frames, dispatch each into an unbounded queue,
/// single worker drains and echoes back. Nothing is rejected.
async fn serve(stream: TcpStream, burst: usize) -> anyhow::Result<()> {
    let limits = FrameLimits::default();
    let (mut reader, mut writer) = stream.into_split();

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
    while received < burst {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        loop {
            if buf.len() < LENGTH_PREFIX_SIZE {
                break;
            }
            let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
            prefix.copy_from_slice(&buf[..LENGTH_PREFIX_SIZE]);
            let body_len = parse_length_prefix(prefix, &limits)?;
            let total = LENGTH_PREFIX_SIZE + body_len;
            if buf.len() < total {
                break;
            }
            let body = buf[LENGTH_PREFIX_SIZE..total].to_vec();
            buf.drain(..total);
            let frame = decode_body(&body)?;
            if frame.kind != FrameKind::Request {
                anyhow::bail!("unexpected frame kind on server: {:?}", frame.kind);
            }
            received += 1;
            // Always accept — unbounded queue absorbs everything.
            let _ = tx.send(frame);
        }
    }
    drop(tx);
    writer_task.await?;
    Ok(())
}

/// Client: open one connection, write the whole burst, read replies,
/// classify into a `Report`.
async fn drive_client(addr: SocketAddr, burst: usize) -> anyhow::Result<Report> {
    let limits = FrameLimits::default();
    let mut request_bytes = Vec::new();
    for id in 1..=burst as u64 {
        let frame = Frame::request(id, "echo", "ping", b"hi".to_vec());
        request_bytes.extend(encode(&frame, &limits)?);
    }

    let stream = TcpStream::connect(addr).await?;
    let (mut reader, mut writer) = stream.into_split();
    writer.write_all(&request_bytes).await?;

    let mut report = Report::default();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while report.total() < burst {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => {
                report.other += burst - report.total();
                return Ok(report);
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        loop {
            if buf.len() < LENGTH_PREFIX_SIZE {
                break;
            }
            let mut prefix = [0u8; LENGTH_PREFIX_SIZE];
            prefix.copy_from_slice(&buf[..LENGTH_PREFIX_SIZE]);
            let body_len = match parse_length_prefix(prefix, &limits) {
                Ok(len) => len,
                Err(_) => {
                    report.other += 1;
                    return Ok(report);
                }
            };
            let total = LENGTH_PREFIX_SIZE + body_len;
            if buf.len() < total {
                break;
            }
            let body = buf[LENGTH_PREFIX_SIZE..total].to_vec();
            buf.drain(..total);
            match decode_body(&body) {
                Ok(frame) => match (frame.kind, frame.error) {
                    (FrameKind::Reply, _) => report.ok += 1,
                    (FrameKind::Error, Some(FrameError::Full)) => report.full += 1,
                    _ => report.other += 1,
                },
                Err(_) => report.other += 1,
            }
        }
    }
    Ok(report)
}
