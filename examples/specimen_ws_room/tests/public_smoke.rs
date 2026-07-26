//! Public runner proof for the WebSocket room specimen.
//!
//! Public smoke exercises the documented Tina path (README `-- tina` →
//! `tina_impl::run()`). Characterization pins the exact two-frame
//! broadcast transcript each client must collect. Order within one inbox
//! depends on broadcast scheduling, so the sorted transcript is pinned
//! instead of the raw vector.

use specimen_ws_room::{Report, tina_impl};

fn sorted_transcript(inbox: &[String]) -> Vec<&str> {
    let mut frames: Vec<&str> = inbox.iter().map(String::as_str).collect();
    frames.sort_unstable();
    frames
}

fn assert_room_report(report: &Report) {
    report.assert_expected();
    assert_eq!(
        sorted_transcript(&report.alpha_inbox),
        ["from-alpha", "from-bravo"],
        "alpha must collect exactly the two published frames"
    );
    assert_eq!(
        sorted_transcript(&report.bravo_inbox),
        ["from-alpha", "from-bravo"],
        "bravo must collect exactly the two published frames"
    );
}

/// Documented public runner path: `tina_impl::run()`.
#[test]
fn public_smoke() {
    assert_room_report(&tina_impl::run().expect("tina side ran"));
}

/// Pins the exact broadcast transcript: two clients, one text frame
/// each, both clients see both frames.
#[test]
fn public_characterization() {
    assert_room_report(&tina_impl::run().expect("tina side ran"));
}
