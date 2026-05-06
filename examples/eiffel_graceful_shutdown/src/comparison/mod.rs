pub(crate) mod tina_impl;
pub(crate) mod tokio_impl;

// Producer wants to push this many work items, spaced out in time. The
// signal arrives partway through; the producer should stop pushing new
// items, but anything already in flight should still be processed.
pub(crate) const TOTAL_PLANNED_ITEMS: u32 = 12;
pub(crate) const ITEM_INTERVAL_MS: u64 = 5;
pub(crate) const SIGNAL_AFTER_MS: u64 = 28;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SideReport {
    pub items_produced: u32,
    pub items_processed: u32,
    pub signal_received: bool,
    pub items_remaining_in_queue_at_exit: u32,
    pub exit_clean: bool,
}

pub(crate) fn print_human_report(side: &str, report: &SideReport) {
    println!(
        "side={side} items_produced={} items_processed={} signal_received={} \
         remaining_in_queue={} exit_clean={}",
        report.items_produced,
        report.items_processed,
        report.signal_received,
        report.items_remaining_in_queue_at_exit,
        report.exit_clean,
    );
}

const MACHINE_PREFIX: &str = "MACHINE_REPORT ";

pub(crate) fn print_machine_report(side: &str, report: &SideReport) {
    println!(
        "{MACHINE_PREFIX}side={side} produced={} processed={} signal={} remaining={} clean={}",
        report.items_produced,
        report.items_processed,
        report.signal_received,
        report.items_remaining_in_queue_at_exit,
        report.exit_clean,
    );
}

pub(crate) fn parse_machine_report(stdout: &str) -> Option<SideReport> {
    let line = stdout
        .lines()
        .find(|line| line.starts_with(MACHINE_PREFIX))?;
    let rest = line.strip_prefix(MACHINE_PREFIX).expect("strip prefix");

    let mut produced = None;
    let mut processed = None;
    let mut signal = None;
    let mut remaining = None;
    let mut clean = None;
    for token in rest.split_whitespace() {
        let (k, v) = token.split_once('=')?;
        match k {
            "produced" => produced = v.parse().ok(),
            "processed" => processed = v.parse().ok(),
            "signal" => signal = v.parse().ok(),
            "remaining" => remaining = v.parse().ok(),
            "clean" => clean = v.parse().ok(),
            _ => {}
        }
    }
    Some(SideReport {
        items_produced: produced?,
        items_processed: processed?,
        signal_received: signal?,
        items_remaining_in_queue_at_exit: remaining?,
        exit_clean: clean?,
    })
}

pub(crate) fn assert_equivalent(tokio: &SideReport, tina: &SideReport) {
    assert!(tokio.signal_received, "tokio observed SIGINT");
    assert!(tina.signal_received, "tina observed SIGINT");

    // Both sides should drain whatever was queued; no items should remain
    // unprocessed at exit.
    assert_eq!(
        tokio.items_remaining_in_queue_at_exit, 0,
        "tokio drained pending work"
    );
    assert_eq!(
        tina.items_remaining_in_queue_at_exit, 0,
        "tina drained pending work"
    );

    // Producer stopped after signal: produced should be > 0 and <= total.
    assert!(
        tokio.items_produced > 0 && tokio.items_produced <= TOTAL_PLANNED_ITEMS,
        "tokio produced some items but stopped on signal"
    );
    assert!(
        tina.items_produced > 0 && tina.items_produced <= TOTAL_PLANNED_ITEMS,
        "tina produced some items but stopped on signal"
    );

    // Whatever was produced should also be processed.
    assert_eq!(
        tokio.items_produced, tokio.items_processed,
        "tokio processed every item it produced"
    );
    assert_eq!(
        tina.items_produced, tina.items_processed,
        "tina processed every item it produced"
    );

    assert!(tokio.exit_clean, "tokio exited cleanly");
    assert!(tina.exit_clean, "tina exited cleanly");
}
