//! Public runner proof for the TCP echo specimen.
//!
//! Characterization pins the echo protocol (bytes echoed identical) and
//! the bounded-mailbox load-shed contract. Public smoke exercises the
//! documented binary paths: the standing server (stdin closed, so it
//! starts and stops cleanly) and the self-terminating `load-shed` demo.

use std::process::{Command, Stdio};

use specimen_tcp_echo::{echo_round_trip, run_load_shed};

/// README runner path: `cargo run ... -- load-shed` exits 0 and prints
/// the admitted/Full accounting; `cargo run ...` (serve) with stdin at
/// EOF starts, prints its bound address, and shuts down cleanly.
#[test]
fn public_smoke() {
    let load_shed = Command::new(env!("CARGO_BIN_EXE_specimen-tcp-echo"))
        .arg("load-shed")
        .output()
        .expect("run load-shed binary");
    assert!(
        load_shed.status.success(),
        "load-shed runner failed: {}",
        String::from_utf8_lossy(&load_shed.stderr),
    );
    let stdout = String::from_utf8_lossy(&load_shed.stdout);
    assert!(
        stdout.contains("load shed: burst=32 cap=4 -> admitted="),
        "load-shed output must report the bounded accounting, got:\n{stdout}",
    );
    assert!(
        stdout.contains("Full="),
        "load-shed output must name the typed Full surplus, got:\n{stdout}",
    );

    let mut serve = Command::new(env!("CARGO_BIN_EXE_specimen-tcp-echo"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve binary");
    // Closing stdin is the scripted equivalent of pressing Enter: the
    // server starts, observes EOF, and shuts down through the bounded
    // terminal path.
    drop(serve.stdin.take());
    let served = serve.wait_with_output().expect("wait for serve binary");
    assert!(
        served.status.success(),
        "serve runner failed: {}",
        String::from_utf8_lossy(&served.stderr),
    );
    let serve_stdout = String::from_utf8_lossy(&served.stdout);
    assert!(
        serve_stdout.contains("echo server listening on 127.0.0.1:"),
        "serve output must print the bound loopback address, got:\n{serve_stdout}",
    );
}

/// Pins the echo protocol and the bounded-mailbox contract.
#[test]
fn public_characterization() {
    let payload = b"the quick brown llama echoes over 127.0.0.1";
    let echoed = echo_round_trip(payload).expect("echo round trip ran");
    assert_eq!(echoed, payload, "echoed bytes must equal sent bytes");

    // A single byte still round-trips; the read/echo/read loop
    // terminates on the client half-close EOF.
    let echoed = echo_round_trip(b"x").expect("short echo round trip ran");
    assert_eq!(echoed, b"x");

    // Bounded mailbox: capacity 4, burst 32 — every record accounted
    // for and the surplus sheds as typed `Full`, never queued.
    let report = run_load_shed(32, 4).expect("load shed ran");
    assert_eq!(report.total(), 32, "every burst record accounted for");
    assert!(report.full > 0, "surplus must shed as typed Full");
    assert_eq!(report.admitted + report.full, 32);
}
