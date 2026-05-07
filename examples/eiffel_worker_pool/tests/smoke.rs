use eiffel_worker_pool::{CLIENTS, tina_impl, tokio_impl};

#[test]
fn tokio_smoke() {
    let r = tokio_impl::run().expect("tokio side ran");
    assert_eq!(r.clients, CLIENTS);
    assert_eq!(r.correct_replies, CLIENTS);
    assert_eq!(r.wrong_replies, 0);
    assert_eq!(r.failed, 0);
    assert!(r.exit_clean);
}

#[test]
fn tina_smoke() {
    let r = tina_impl::run().expect("tina side ran");
    assert_eq!(r.clients, CLIENTS);
    assert_eq!(r.correct_replies, CLIENTS);
    assert_eq!(r.wrong_replies, 0);
    assert_eq!(r.failed, 0);
    assert!(r.exit_clean);
}
