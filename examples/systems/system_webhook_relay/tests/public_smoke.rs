//! Public runner proof for the webhook relay system.
//!
//! Public smoke drives the exact configuration the documented binary
//! (`cargo run -- ...`, see `src/main.rs`) submits to `run`. The hermetic
//! fake outbound consumes one scripted outcome per event in submit order,
//! so characterization pins the exact reply vector and counters.

use system_webhook_relay::{
    DeadLetterReason, DriverReply, FakeOutboundProgram, OutboundError, RelayReply, RunConfig, run,
};
use tina_aws_bridge::{BridgeFatal, BridgeOutcomeClass, BridgeRetryable};

/// The exact program `src/main.rs` runs.
fn documented_config() -> RunConfig {
    RunConfig {
        events: 4,
        call_timeout_ms: 2_000,
        program: vec![
            FakeOutboundProgram::Deliver("backend-1".into()),
            FakeOutboundProgram::Fail(OutboundError::Throttled),
            FakeOutboundProgram::Fail(OutboundError::NotFound),
            FakeOutboundProgram::Deliver("backend-2".into()),
        ],
    }
}

fn expected_replies() -> Vec<DriverReply> {
    vec![
        DriverReply::Reply(RelayReply::Delivered {
            backend_id: "backend-1".into(),
        }),
        DriverReply::Reply(RelayReply::Retry {
            reason: BridgeRetryable::ServiceThrottled,
        }),
        DriverReply::Reply(RelayReply::DeadLetter {
            reason: DeadLetterReason::Fatal(BridgeFatal::NotFound),
        }),
        DriverReply::Reply(RelayReply::Delivered {
            backend_id: "backend-2".into(),
        }),
    ]
}

/// Documented public runner path: `run` with the binary's program.
#[test]
fn public_smoke() {
    let report = run(documented_config()).expect("relay run");
    assert_eq!(report.replies, expected_replies(), "{report:?}");
    assert_eq!(report.stats.delivered, 2, "{report:?}");
    assert_eq!(report.stats.transient, 1, "{report:?}");
    assert_eq!(report.stats.dead_letter, 1, "{report:?}");
}

/// Pins the exact classification transcript of the documented program
/// and the classifier mapping the relay relies on.
#[test]
fn public_characterization() {
    assert_eq!(
        OutboundError::Throttled.classify(),
        BridgeOutcomeClass::Retryable(BridgeRetryable::ServiceThrottled)
    );
    assert_eq!(
        OutboundError::NotFound.classify(),
        BridgeOutcomeClass::Fatal(BridgeFatal::NotFound)
    );

    let report = run(documented_config()).expect("characterization run");
    assert_eq!(report.replies, expected_replies(), "{report:?}");
    assert_eq!(report.stats.delivered, 2, "{report:?}");
    assert_eq!(report.stats.transient, 1, "{report:?}");
    assert_eq!(report.stats.dead_letter, 1, "{report:?}");
}
