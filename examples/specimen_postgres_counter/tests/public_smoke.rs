//! Public runner proof for the Postgres counter specimen.
//!
//! Characterization pins the increment arithmetic. Public smoke
//! exercises the documented Tina path through `tina-sqlx-bridge`.
//! Without `DATABASE_URL` both tests print the specimen's skip
//! notice and pass, matching the `src/main.rs` skip path.

use specimen_postgres_counter::{INCREMENTS, Report, database_url, tina_impl};

fn assert_incremented(report: Report) {
    assert_eq!(report.final_value, INCREMENTS as u64);
    assert!(report.exit_clean);
}

fn run_public() -> anyhow::Result<()> {
    let url = match database_url() {
        Some(url) => url,
        None => {
            println!(
                "specimen_postgres_counter: skipped (DATABASE_URL not set; \
                 see README for the expected env var)"
            );
            return Ok(());
        }
    };
    assert_incremented(tina_impl::run(&url)?);
    Ok(())
}

/// Pins increment arithmetic before/after host-result migration.
#[test]
fn public_characterization() -> anyhow::Result<()> {
    run_public()
}

/// Documented public runner path: `tina_impl::run(&url)`.
#[test]
fn public_smoke() -> anyhow::Result<()> {
    run_public()
}
