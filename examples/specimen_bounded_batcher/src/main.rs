use specimen_bounded_batcher::{Report, tina_impl, tokio_impl};

fn main() -> anyhow::Result<()> {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "both".to_string());
    match mode.as_str() {
        "tokio" => print_side("tokio", tokio_impl::run()?),
        "tina" => print_side("tina", tina_impl::run()?),
        "both" => {
            print_side("tokio", tokio_impl::run()?);
            print_side("tina", tina_impl::run()?);
        }
        other => anyhow::bail!("unknown mode {other:?}"),
    }
    Ok(())
}

fn print_side(side: &str, r: Report) {
    println!(
        "comparison=specimen_bounded_batcher side={} callers={} successes={} \
         full={} failed={} transport_full={} closed={} timeouts={} rejected={} timer_failures={} \
         host_foreign_system={} host_parent_stopped={} host_command_full={} host_worker_stopped={} host_wait_timeout={} \
         host_worker_unresponsive={} host_unknown_shard={} host_driver_shutdown_failed={} \
         host_driver_park_failed={} \
         size_flushes={} timer_flushes={} exit_clean={}",
        side,
        r.callers,
        r.successes,
        r.full_rejects,
        r.failed,
        r.transport_full,
        r.closed,
        r.timeouts,
        r.rejected,
        r.timer_failures,
        r.host_foreign_system,
        r.host_parent_stopped,
        r.host_command_full,
        r.host_worker_stopped,
        r.host_wait_timeout,
        r.host_worker_unresponsive,
        r.host_unknown_shard,
        r.host_driver_shutdown_failed,
        r.host_driver_park_failed,
        r.batches_size_flushed,
        r.batches_timer_flushed,
        r.exit_clean,
    );
}
