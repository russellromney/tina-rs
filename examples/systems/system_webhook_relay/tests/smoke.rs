use system_webhook_relay::{
    DeadLetterReason, DriverReply, FakeOutboundProgram, OutboundError, RelayReply, RunConfig, run,
};
use tina_aws_bridge::{BridgeFatal, BridgeRetryable, BridgeUnavailable};

fn reply(d: &DriverReply) -> &RelayReply {
    match d {
        DriverReply::Reply(r) => r,
        other => panic!("expected reply, got {other:?}"),
    }
}

#[test]
fn delivered_throttled_and_dead_letter_are_classified_correctly() {
    let report = run(RunConfig {
        events: 4,
        call_timeout_ms: 2_000,
        program: vec![
            FakeOutboundProgram::Deliver("backend-1".into()),
            FakeOutboundProgram::Fail(OutboundError::Throttled),
            FakeOutboundProgram::Fail(OutboundError::NotFound),
            FakeOutboundProgram::Deliver("backend-2".into()),
        ],
    })
    .expect("run succeeds");

    assert_eq!(report.replies.len(), 4);
    assert!(matches!(
        reply(&report.replies[0]),
        RelayReply::Delivered { backend_id } if backend_id == "backend-1"
    ));
    assert!(matches!(
        reply(&report.replies[1]),
        RelayReply::Retry {
            reason: BridgeRetryable::ServiceThrottled
        }
    ));
    assert!(matches!(
        reply(&report.replies[2]),
        RelayReply::DeadLetter {
            reason: DeadLetterReason::Fatal(BridgeFatal::NotFound)
        }
    ));
    assert!(matches!(
        reply(&report.replies[3]),
        RelayReply::Delivered { backend_id } if backend_id == "backend-2"
    ));

    assert_eq!(report.stats.delivered, 2);
    assert_eq!(report.stats.transient, 1);
    assert_eq!(report.stats.dead_letter, 1);
}

#[test]
fn full_is_retryable_and_closed_is_unavailable() {
    let report = run(RunConfig {
        events: 2,
        call_timeout_ms: 2_000,
        program: vec![
            FakeOutboundProgram::Fail(OutboundError::Full),
            FakeOutboundProgram::Fail(OutboundError::Closed),
        ],
    })
    .expect("run succeeds");

    assert!(matches!(
        reply(&report.replies[0]),
        RelayReply::Retry {
            reason: BridgeRetryable::BridgeFull
        }
    ));
    // `Closed` is `Unavailable` per the bridge classifier rules: retrying
    // on the same handle reproduces the failure, so the relay dead-letters
    // and the caller must rebuild the bridge.
    assert!(matches!(
        reply(&report.replies[1]),
        RelayReply::DeadLetter {
            reason: DeadLetterReason::Unavailable(BridgeUnavailable::BridgeClosed)
        }
    ));
    assert_eq!(report.stats.transient, 1);
    assert_eq!(report.stats.dead_letter, 1);
}

#[test]
fn invalid_parameter_and_access_denied_are_dead_letter() {
    let report = run(RunConfig {
        events: 2,
        call_timeout_ms: 2_000,
        program: vec![
            FakeOutboundProgram::Fail(OutboundError::InvalidParameter),
            FakeOutboundProgram::Fail(OutboundError::AccessDenied),
        ],
    })
    .expect("run succeeds");

    assert!(matches!(
        reply(&report.replies[0]),
        RelayReply::DeadLetter {
            reason: DeadLetterReason::Fatal(BridgeFatal::InvalidParameter)
        }
    ));
    assert!(matches!(
        reply(&report.replies[1]),
        RelayReply::DeadLetter {
            reason: DeadLetterReason::Fatal(BridgeFatal::AccessDenied)
        }
    ));
    assert_eq!(report.stats.dead_letter, 2);
    assert_eq!(report.stats.transient, 0);
}

#[test]
fn caller_timeout_settles_before_bounded_shutdown() {
    let report = run(RunConfig {
        events: 1,
        call_timeout_ms: 0,
        program: vec![FakeOutboundProgram::Deliver("too-late".into())],
    })
    .expect("caller-gone completion and shutdown must settle");

    assert_eq!(report.replies, vec![DriverReply::OuterTimeout]);
    assert_eq!(report.stats.delivered, 0);
    assert_eq!(report.stats.transient, 1);
    assert_eq!(report.stats.dead_letter, 0);
}
