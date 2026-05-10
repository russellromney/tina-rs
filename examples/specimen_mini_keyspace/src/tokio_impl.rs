//! Tokio reference: real loopback TCP, an `async` block, a
//! `BTreeMap` on the stack. State ownership is implicit in "the task
//! that happens to be running" — easy to write, easy to widen with
//! `Arc<Mutex<_>>` later.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::thread;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{Command, Report, SCRIPT, parse_commands};

pub fn run() -> anyhow::Result<Report> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;

    runtime.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let client = thread::spawn(move || drive_client(addr));

        let (mut stream, _peer) = listener.accept().await?;
        let mut request = Vec::new();
        stream.read_to_end(&mut request).await?;

        let mut store: BTreeMap<String, String> = BTreeMap::new();
        let mut response = Vec::new();
        let mut report = Report::default();
        for command in parse_commands(&request)? {
            match command {
                Command::Set { key, value } => {
                    store.insert(key, value);
                    response.extend_from_slice(b"OK\n");
                    report.ok += 1;
                }
                Command::Get { key } => match store.get(&key) {
                    Some(value) => {
                        response.extend_from_slice(format!("VALUE {value}\n").as_bytes());
                        report.values += 1;
                    }
                    None => {
                        response.extend_from_slice(b"MISS\n");
                        report.misses += 1;
                    }
                },
                Command::Del { key } => {
                    if store.remove(&key).is_some() {
                        response.extend_from_slice(b"DELETED\n");
                        report.deleted += 1;
                    } else {
                        response.extend_from_slice(b"MISS\n");
                        report.misses += 1;
                    }
                }
                Command::Quit => response.extend_from_slice(b"BYE\n"),
            }
        }

        stream.write_all(&response).await?;
        drop(stream);
        client.join().expect("client thread")?;
        Ok(report)
    })
}

fn drive_client(addr: SocketAddr) -> anyhow::Result<()> {
    let mut stream = std::net::TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(SCRIPT.as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut sink = Vec::new();
    stream.read_to_end(&mut sink)?;
    Ok(())
}
