pub(crate) mod tina_impl;
pub(crate) mod tokio_impl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SideReport {
    pub successful_get: u32,
    pub successful_post: u32,
    pub final_counter_value: u32,
    pub got_404_for_missing: bool,
    pub exit_clean: bool,
}

pub(crate) fn print_report(side: &str, report: &SideReport) {
    println!(
        "side={side} successful_get={} successful_post={} \
         final_counter_value={} got_404_for_missing={} exit_clean={}",
        report.successful_get,
        report.successful_post,
        report.final_counter_value,
        report.got_404_for_missing,
        report.exit_clean,
    );
}

pub(crate) fn assert_equivalent(tokio: &SideReport, tina: &SideReport) {
    // Same scripted client on both sides: 1 GET (returns 0), 3 POSTs
    // (return 1, 2, 3), 1 GET (returns 3), 1 GET /missing (404).
    assert_eq!(tokio.successful_get, 2, "tokio served two GETs");
    assert_eq!(tina.successful_get, 2, "tina served two GETs");
    assert_eq!(tokio.successful_post, 3, "tokio served three POSTs");
    assert_eq!(tina.successful_post, 3, "tina served three POSTs");
    assert_eq!(
        tokio.final_counter_value, 3,
        "tokio counter ended at 3 after three POSTs"
    );
    assert_eq!(
        tina.final_counter_value, 3,
        "tina counter ended at 3 after three POSTs"
    );
    assert!(
        tokio.got_404_for_missing,
        "tokio returned 404 for unknown path"
    );
    assert!(
        tina.got_404_for_missing,
        "tina returned 404 for unknown path"
    );
    assert!(tokio.exit_clean, "tokio exited cleanly");
    assert!(tina.exit_clean, "tina exited cleanly");
}
