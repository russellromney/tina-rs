use specimen_bounded_batcher::{CALLERS, Report, tina_impl, tokio_impl};

fn assert_shape(side: &str, r: Report) {
    assert_eq!(r.callers, CALLERS);
    assert_eq!(r.successes + r.full_rejects + r.failed, CALLERS);
    assert_eq!(r.failed, 0, "{side}: {r:?}");
    assert_eq!(r.transport_full, 0, "{side}: {r:?}");
    assert_eq!(r.closed, 0, "{side}: {r:?}");
    assert_eq!(r.timeouts, 0, "{side}: {r:?}");
    assert_eq!(r.rejected, 0, "{side}: {r:?}");
    assert_eq!(r.timer_failures, 0, "{side}: {r:?}");
    assert!(
        r.batches_size_flushed + r.batches_timer_flushed > 0,
        "{side}: expected at least one flush, got {r:?}",
    );
    assert!(r.exit_clean);
}

#[test]
fn tokio_smoke() {
    assert_shape("tokio", tokio_impl::run().expect("tokio side ran"));
}

#[test]
fn tina_smoke() {
    assert_shape("tina", tina_impl::run().expect("tina side ran"));
}
