//! Tokio reference: `TcpStream::connect` + `write_all` +
//! `read_to_end` per fetch, in a `for` loop on a current_thread
//! runtime.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Builder;

use crate::{FETCH_COUNT, RESPONSE, Report, TestServer};

pub fn run() -> anyhow::Result<Report> {
    let server = TestServer::start(FETCH_COUNT)?;
    let addr = server.addr;
    let runtime = Builder::new_current_thread().enable_all().build()?;

    let report = runtime.block_on(async move {
        let mut report = Report::default();
        for _ in 0..FETCH_COUNT {
            match fetch_one(addr).await {
                Ok(received) if received == RESPONSE => {
                    report.successful_fetches += 1;
                    report.bytes_received += received.len();
                }
                _ => report.failed_fetches += 1,
            }
        }
        report.exit_clean = true;
        report
    });

    drop(server);
    Ok(report)
}

async fn fetch_one(addr: std::net::SocketAddr) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(b"GET\n").await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(buf)
}
