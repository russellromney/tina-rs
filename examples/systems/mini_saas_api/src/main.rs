use std::net::SocketAddr;

use mini_saas_api::{RunMode, run, serve};

const DEFAULT_ADDR: &str = "127.0.0.1:8080";

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        // Run-forever entrypoint: bind and serve until a signal arrives.
        Some("serve") => {
            let addr = parse_serve_addr(args)?;
            serve(addr)
        }
        None | Some("smoke") => run_scripted(RunMode::Smoke),
        Some("pressure") => run_scripted(RunMode::Pressure),
        Some(other) => anyhow::bail!(
            "unknown mode {other:?}; expected `serve`, `smoke`, or `pressure`. \
             usage: mini-saas-api [serve [--addr HOST:PORT] | smoke | pressure]"
        ),
    }
}

fn run_scripted(mode: RunMode) -> anyhow::Result<()> {
    let report = run(mode)?;
    println!("{}", report.summary_line());
    println!("{}", report.capacity_before_shutdown_line.trim_end());
    println!("{}", report.capacity_during_shutdown_line.trim_end());
    println!("{}", report.terminal_line);
    println!("live_replay_fact {}", report.live_replay_fact);
    Ok(())
}

/// Parse the optional `--addr HOST:PORT` flag; defaults to `127.0.0.1:8080`.
fn parse_serve_addr(mut args: impl Iterator<Item = String>) -> anyhow::Result<SocketAddr> {
    let addr = match args.next().as_deref() {
        None => DEFAULT_ADDR.to_owned(),
        Some("--addr") => args
            .next()
            .ok_or_else(|| anyhow::anyhow!("--addr requires a HOST:PORT value"))?,
        Some(other) => anyhow::bail!(
            "unexpected serve argument {other:?}; usage: serve [--addr HOST:PORT]"
        ),
    };
    addr.parse::<SocketAddr>()
        .map_err(|e| anyhow::anyhow!("invalid --addr {addr:?}: {e}"))
}
