//! Runnable demo: one echo round trip over a real loopback socket,
//! then one bounded-mailbox load-shed line.
//!
//! ```sh
//! cargo run --manifest-path examples/specimen_tcp_echo/Cargo.toml
//! ```

use specimen_tcp_echo::{LISTENER_CAPACITY, echo_round_trip, run_load_shed};

const LOAD_SHED_CAPACITY: usize = 4;
const LOAD_SHED_BURST: u32 = 32;

fn main() -> anyhow::Result<()> {
    // One connection, one isolate, one echo.
    let payload = b"hello from the tina-rs tcp echo server";
    let echoed = echo_round_trip(payload)?;
    assert_eq!(echoed, payload, "echo must return the identical bytes");
    println!("echo: sent {} bytes, got them back unchanged", echoed.len());

    // The bounded-mailbox contract, shown directly: a producer bursts a
    // worker whose mailbox holds only LOAD_SHED_CAPACITY records. The
    // surplus comes back as a typed Full instead of an unbounded queue.
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
