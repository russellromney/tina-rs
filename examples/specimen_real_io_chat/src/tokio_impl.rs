//! Tokio reference: real loopback TCP, unbounded `mpsc` queue, single
//! slow consumer that drains exactly one message. Every fanout
//! attempt succeeds at admission; the rest sit silently in the
//! channel's buffer.

use std::net::SocketAddr;
use std::thread;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::{Report, RunConfig};

pub fn run(config: RunConfig) -> anyhow::Result<Report> {
    let config = config.validate()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;

    runtime.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let burst = config.burst;

        // Drive real TCP from a separate (std) thread so the server
        // task can `accept` against a live client.
        let client = thread::spawn(move || drive_client(addr, burst));

        let (mut stream, _peer) = listener.accept().await?;

        // Read the requested burst from the wire (the client sends it
        // and shuts down its write side). The server then runs the
        // workload and writes a single "done" byte so the client can
        // read to EOF and unblock.
        let mut request = Vec::new();
        stream.read_to_end(&mut request).await?;
        let requested_burst = parse_burst(&request)?;

        // Unbounded queue + slow single consumer: every send_observed
        // succeeds, and we drain exactly once.
        let (tx, mut rx) = mpsc::unbounded_channel::<usize>();
        let mut accepted = 0usize;
        for index in 0..requested_burst {
            if tx.send(index).is_ok() {
                accepted += 1;
            }
        }
        let delivered = usize::from(rx.recv().await.is_some());
        let buffered = accepted.saturating_sub(delivered);

        stream.write_all(b"done\n").await?;
        drop(stream);
        client
            .join()
            .map_err(|_| anyhow::anyhow!("client thread panicked"))??;

        Ok(Report {
            accepted,
            full: 0,
            closed: 0,
            delivered,
            buffered,
        })
    })
}

/// Connect, write the requested burst, shut down the write side,
/// drain the response to EOF.
fn drive_client(addr: SocketAddr, burst: usize) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpStream as StdTcpStream;
    use std::time::Duration;

    let mut stream = StdTcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(burst.to_string().as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut sink = Vec::new();
    stream.read_to_end(&mut sink)?;
    Ok(())
}

fn parse_burst(bytes: &[u8]) -> anyhow::Result<usize> {
    Ok(std::str::from_utf8(bytes)?.trim().parse::<usize>()?)
}
