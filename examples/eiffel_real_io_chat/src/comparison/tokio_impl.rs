use std::thread;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener as TokioTcpListener;
use tokio::sync::mpsc;

use super::{SideReport, format_response, parse_burst, parse_response, real_tcp_client};

pub(crate) fn run(burst: usize) -> SideReport {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async move {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tokio listener");
        let addr = listener.local_addr().expect("tokio listener local addr");
        let client = thread::spawn(move || real_tcp_client(addr, burst));

        let (mut stream, _peer_addr) = listener.accept().await.expect("tokio accept");
        let mut request = Vec::new();
        stream
            .read_to_end(&mut request)
            .await
            .expect("tokio read request");
        let requested_burst = parse_burst(&request);

        let (tx, mut rx) = mpsc::unbounded_channel::<usize>();
        let mut accepted = 0usize;
        let full = 0;
        let closed = 0;
        for index in 0..requested_burst {
            if tx.send(index).is_ok() {
                accepted += 1;
            }
        }

        let delivered = usize::from(rx.recv().await.is_some());
        let buffered = accepted.saturating_sub(delivered);
        let response = format_response(accepted, full, closed, delivered, buffered);
        stream
            .write_all(&response)
            .await
            .expect("tokio write response");
        drop(stream);

        let client_response = client.join().expect("tokio client thread");
        assert_eq!(client_response, response);
        parse_response(client_response, true, false)
    })
}
