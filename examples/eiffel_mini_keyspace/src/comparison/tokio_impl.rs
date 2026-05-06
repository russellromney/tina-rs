use std::collections::BTreeMap;
use std::thread;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener as TokioTcpListener;

use super::{Command, report_from_response, scripted_client};

pub(crate) fn run() -> super::SideReport {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tokio listener");
        let addr = listener.local_addr().expect("tokio listener local addr");
        let client = thread::spawn(move || scripted_client(addr));

        let (mut stream, _peer_addr) = listener.accept().await.expect("tokio accept");
        let mut request = Vec::new();
        stream
            .read_to_end(&mut request)
            .await
            .expect("tokio read request");

        let mut store = BTreeMap::<String, String>::new();
        let mut response = Vec::new();
        for command in super::parse_commands(&request) {
            match command {
                Command::Set { key, value } => {
                    store.insert(key, value);
                    response.extend_from_slice(b"OK\n");
                }
                Command::Get { key } => {
                    if let Some(value) = store.get(&key) {
                        response.extend_from_slice(format!("VALUE {value}\n").as_bytes());
                    } else {
                        response.extend_from_slice(b"MISS\n");
                    }
                }
                Command::Del { key } => {
                    if store.remove(&key).is_some() {
                        response.extend_from_slice(b"DELETED\n");
                    } else {
                        response.extend_from_slice(b"MISS\n");
                    }
                }
                Command::Quit => response.extend_from_slice(b"BYE\n"),
            }
        }

        stream
            .write_all(&response)
            .await
            .expect("tokio write response");
        drop(stream);

        let client_response = client.join().expect("tokio client thread");
        assert_eq!(client_response, response);
        report_from_response(client_response)
    })
}
