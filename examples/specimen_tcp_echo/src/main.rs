//! A standing TCP echo server you run and connect to.
//!
//! Default (no argument) starts a real server: it binds an ephemeral
//! loopback port, prints the address, and accepts forever until you
//! press Enter. Connect a second terminal with `nc` and every line you
//! type comes straight back.
//!
//! ```sh
//! cargo run --manifest-path examples/specimen_tcp_echo/Cargo.toml
//! # then, in another terminal:
//! #   nc 127.0.0.1 <port>
//! ```
//!
//! The `load-shed` subcommand runs the self-terminating bounded-mailbox
//! demo instead: a host producer bursts a bounded worker and the surplus
//! comes back as a typed `Full`.
//!
//! ```sh
//! cargo run --manifest-path examples/specimen_tcp_echo/Cargo.toml -- load-shed
//! ```

use std::io::BufRead;
use std::net::SocketAddr;
use std::time::Duration;

use specimen_tcp_echo::{EchoListener, EchoListenerMsg, LISTENER_CAPACITY, run_load_shed};
use tina::prelude::SingleShard;
use tina_runtime::{DefaultThreadedMailboxFactory, LocalSystem};

const LOAD_SHED_CAPACITY: usize = 4;
const LOAD_SHED_BURST: u32 = 32;

fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => serve(),
        Some("load-shed") => load_shed(),
        Some(other) => {
            eprintln!("unknown argument {other:?}; usage: specimen-tcp-echo [load-shed]");
            std::process::exit(2);
        }
    }
}

/// Runs the standing echo server until the operator presses Enter.
///
/// The listener accepts forever (`target_accepts = None`), so this is a
/// real server, not a bounded test. The bound address is learned through
/// the system's `observe_next_bound` handle — no shared-slot polling.
fn serve() -> anyhow::Result<()> {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory).try_build()?;
    app.run_to_shutdown_reported(Duration::from_secs(5), serve_until_enter)
        .map_err(|e| anyhow::anyhow!("echo server: {e:?}"))?;
    Ok(())
}

fn serve_until_enter(
    app: &LocalSystem<SingleShard, DefaultThreadedMailboxFactory>,
) -> anyhow::Result<()> {
    let bind_addr: SocketAddr = "127.0.0.1:0".parse()?;

    let listener = app
        .register_root::<EchoListener, EchoListenerMsg>(
            EchoListener::new(bind_addr, None),
            LISTENER_CAPACITY,
        )
        .map_err(|e| anyhow::anyhow!("register listener: {e:?}"))?;

    let bound = app
        .observe_next_bound()
        .map_err(|e| anyhow::anyhow!("register bind observer: {e}"))?;
    app.try_send(listener, EchoListenerMsg::Start)
        .map_err(|e| anyhow::anyhow!("start listener: {e:?}"))?;

    let addr = bound
        .wait(Duration::from_secs(3))
        .map_err(|e| anyhow::anyhow!("listener bind: {e:?}"))?;

    println!("echo server listening on {addr}");
    println!("connect with:  nc {} {}", addr.ip(), addr.port());
    println!("press Enter to stop");

    // Serve until the operator stops us. Blocking on stdin keeps this a
    // dependency-free standing server; the system's own threads keep
    // accepting and echoing while we wait here.
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(())
}

/// The bounded-mailbox contract, shown directly: a producer bursts a
/// worker whose mailbox holds only `LOAD_SHED_CAPACITY` records. The
/// surplus comes back as a typed `Full` instead of an unbounded queue.
/// This path self-terminates.
fn load_shed() -> anyhow::Result<()> {
    let report = run_load_shed(LOAD_SHED_BURST, LOAD_SHED_CAPACITY)?;
    assert_eq!(
        report.total(),
        LOAD_SHED_BURST,
        "every burst record must be accounted for: {report:?}",
    );
    assert!(
        report.full > 0,
        "a burst past capacity must shed at least one record as Full: {report:?}",
    );
    println!(
        "load shed: burst={} cap={} -> admitted={} Full={} (listener cap for reference: {})",
        LOAD_SHED_BURST, LOAD_SHED_CAPACITY, report.admitted, report.full, LISTENER_CAPACITY,
    );
    Ok(())
}
