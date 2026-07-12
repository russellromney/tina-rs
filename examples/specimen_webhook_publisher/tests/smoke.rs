use specimen_webhook_publisher::{tina_impl, tokio_impl};

#[test]
fn tokio_side_records_three_increments() {
    tokio_impl::run()
        .expect("run Tokio specimen")
        .assert_expected();
}

#[test]
fn tina_side_records_three_increments() {
    tina_impl::run()
        .expect("run Tina specimen")
        .assert_expected();
}
