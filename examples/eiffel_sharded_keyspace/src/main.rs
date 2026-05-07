//! Run both implementations and print their reports.
//!
//! ```bash
//! cargo run -p eiffel-sharded-keyspace
//! ```

use eiffel_sharded_keyspace::{tina_impl, tokio_impl};

fn main() -> anyhow::Result<()> {
    let tokio_report = tokio_impl::run()?;
    println!("tokio: {tokio_report:?}");

    let tina_report = tina_impl::run()?;
    println!("tina:  {tina_report:?}");

    if tokio_report != tina_report {
        anyhow::bail!("tokio and tina reports diverged: {tokio_report:?} vs {tina_report:?}");
    }
    println!("ok: both sides produced the same Report");
    Ok(())
}
