pub(crate) mod tina_impl;
pub(crate) mod tokio_impl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TinaReport {
    pub seed_a: u64,
    pub run_a1_event_count: usize,
    pub run_a2_event_count: usize,
    pub run_a1_fingerprint: u64,
    pub run_a2_fingerprint: u64,
    pub seed_b: u64,
    pub run_b1_event_count: usize,
    pub run_b1_fingerprint: u64,
    pub messages_received: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokioReport {
    pub run1_messages: Vec<u32>,
    pub run2_messages: Vec<u32>,
    pub run1_micros: Vec<u128>,
    pub run2_micros: Vec<u128>,
}

pub(crate) fn print_tina_report(report: &TinaReport) {
    println!(
        "side=tina seed={} run1_events={} run2_events={} fingerprints_match={} \
         seed_b={} run_b1_fingerprint_differs_from_a={} messages_received={}",
        report.seed_a,
        report.run_a1_event_count,
        report.run_a2_event_count,
        report.run_a1_fingerprint == report.run_a2_fingerprint,
        report.seed_b,
        report.run_b1_fingerprint != report.run_a1_fingerprint,
        report.messages_received,
    );
}

pub(crate) fn print_tokio_report(report: &TokioReport) {
    let timings_match = report.run1_micros == report.run2_micros;
    let messages_match = report.run1_messages == report.run2_messages;
    println!(
        "side=tokio messages_match={messages_match} timings_match={timings_match} \
         run1_us={:?} run2_us={:?}",
        report.run1_micros, report.run2_micros,
    );
}

pub(crate) fn assert_replay_distinction(tina: &TinaReport, tokio: &TokioReport) {
    assert_eq!(
        tina.run_a1_event_count, tina.run_a2_event_count,
        "tina event count is identical across replays of the same seed"
    );
    assert_eq!(
        tina.run_a1_fingerprint, tina.run_a2_fingerprint,
        "tina trace fingerprint is byte-identical across replays of the same seed"
    );
    assert_ne!(
        tina.run_a1_fingerprint, tina.run_b1_fingerprint,
        "tina trace fingerprint differs across distinct seeds (so the property is non-trivial)"
    );

    // The Tokio side should agree on the *messages* (correctness) but not on
    // the wall-clock *timings* (no replay story). We assert the weaker thing
    // — messages match, but timings drift — so the test does not flake on
    // unusually quiet systems where both runs happen to land on identical
    // microsecond boundaries.
    assert_eq!(
        tokio.run1_messages, tokio.run2_messages,
        "tokio is functionally correct: same message sequence twice"
    );
}
