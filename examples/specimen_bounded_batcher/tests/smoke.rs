use specimen_bounded_batcher::{
    CALLERS, MAX_PENDING, Report, SUBMISSION_CAPACITY, tina_impl, tokio_impl,
};

const _: () = {
    assert!(CALLERS <= MAX_PENDING);
    assert!(CALLERS <= SUBMISSION_CAPACITY);
};

fn assert_shape(side: &str, r: Report) {
    assert_eq!(r.callers, CALLERS);
    assert_eq!(r.successes + r.full_rejects + r.failed, CALLERS);
    assert_eq!(r.failed, 0, "{side}: {r:?}");
    assert_eq!(r.transport_full, 0, "{side}: {r:?}");
    assert_eq!(r.closed, 0, "{side}: {r:?}");
    assert_eq!(r.timeouts, 0, "{side}: {r:?}");
    assert_eq!(r.rejected, 0, "{side}: {r:?}");
    assert_eq!(r.timer_failures, 0, "{side}: {r:?}");
    assert_eq!(r.host_command_full, 0, "{side}: {r:?}");
    assert_eq!(r.host_worker_stopped, 0, "{side}: {r:?}");
    assert_eq!(r.host_wait_timeout, 0, "{side}: {r:?}");
    assert_eq!(r.host_worker_unresponsive, 0, "{side}: {r:?}");
    assert_eq!(r.host_unknown_shard, 0, "{side}: {r:?}");
    assert_eq!(r.host_driver_shutdown_failed, 0, "{side}: {r:?}");
    assert_eq!(r.host_driver_park_failed, 0, "{side}: {r:?}");
    assert!(
        r.batches_size_flushed + r.batches_timer_flushed > 0,
        "{side}: expected at least one flush, got {r:?}",
    );
    assert!(r.exit_clean);
}

#[test]
fn tokio_and_tina_smoke_preserve_reference_behavior() {
    let tokio = tokio_impl::run().expect("tokio side ran");
    let tina = tina_impl::run().expect("tina side ran");
    assert_shape("tokio", tokio);
    assert_shape("tina", tina);
    assert_eq!(tina.callers, tokio.callers);
    assert_eq!(tina.successes, tokio.successes);
    assert_eq!(tina.full_rejects, tokio.full_rejects);
    assert_eq!(tina.failed, tokio.failed);
    assert_eq!(tina.exit_clean, tokio.exit_clean);
}
