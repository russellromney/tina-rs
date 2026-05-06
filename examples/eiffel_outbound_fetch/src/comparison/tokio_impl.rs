use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Builder;

use super::{FETCH_COUNT, RESPONSE, SideReport, TestServer};

pub(crate) fn run() -> SideReport {
    let server = TestServer::start(FETCH_COUNT);
    let addr = server.addr;

    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let (successful, failed, bytes) = runtime.block_on(async move {
        let mut successful = 0u32;
        let mut failed = 0u32;
        let mut bytes = 0usize;
        for _ in 0..FETCH_COUNT {
            match fetch_one(addr).await {
                Ok(received) => {
                    if received == RESPONSE {
                        successful += 1;
                        bytes += received.len();
                    } else {
                        failed += 1;
                    }
                }
                Err(_) => {
                    failed += 1;
                }
            }
        }
        (successful, failed, bytes)
    });

    drop(server);

    SideReport {
        addr_used: addr,
        successful_fetches: successful,
        failed_fetches: failed,
        bytes_received: bytes,
        exit_clean: true,
    }
}

async fn fetch_one(addr: std::net::SocketAddr) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr).await?;
    // Half-close hint: a real client would send a request line. The test
    // server reads-then-writes, so a small write keeps the shapes honest.
    stream.write_all(b"GET\n").await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    Ok(buf)
}
