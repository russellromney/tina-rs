use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use system_webhook_relay::{
    DeadLetterReason, DriverReply, FakeOutboundProgram, OutboundError, RelayReply,
    RelayWorkloadError, RunConfig, RunConfigError, RunError, run, run_against_sqs,
};
use tina::CallRejectedReason;
use tina_aws_bridge::{
    BridgeFatal, BridgeRetryable, BridgeUnavailable, SqsConfig, SqsConfigError, SqsCredentials,
    SqsInstallError,
};
use tina_runtime::RunToShutdownError;

fn one_shot_http_500() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake SQS");
    listener
        .set_nonblocking(true)
        .expect("make fake SQS accept bounded");
    let address = listener.local_addr().expect("fake SQS address");
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept fake SQS request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set fake SQS timeout");
        let mut request = [0_u8; 8 * 1024];
        let _ = stream.read(&mut request).expect("read fake SQS request");
        stream
            .write_all(
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/x-amz-json-1.0\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            )
            .expect("write fake SQS response");
    });
    (format!("http://{address}"), thread)
}

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
    for _ in 0..20 {
        let report = run(RunConfig {
            events: 1,
            call_timeout_ms: 0,
            program: vec![FakeOutboundProgram::Deliver("too-late".into())],
        })
        .expect("caller-gone completion and bounded shutdown must settle");

        assert_eq!(report.replies, vec![DriverReply::OuterTimeout]);
        assert_eq!(report.stats.delivered, 0);
        assert_eq!(report.stats.transient, 1);
        assert_eq!(report.stats.dead_letter, 0);
    }
}

#[test]
fn rejection_reason_survives_the_outbound_classifier() {
    let report = run(RunConfig {
        events: 1,
        call_timeout_ms: 2_000,
        program: vec![FakeOutboundProgram::Reject(
            CallRejectedReason::UnsupportedMessage,
        )],
    })
    .expect("rejection and shutdown settle");

    assert_eq!(
        report.replies,
        vec![DriverReply::Reply(RelayReply::DeadLetter {
            reason: DeadLetterReason::Rejected(CallRejectedReason::UnsupportedMessage),
        })]
    );
    assert_eq!(report.stats.delivered, 0);
    assert_eq!(report.stats.transient, 0);
    assert_eq!(report.stats.dead_letter, 1);
}

#[test]
fn stopped_outbound_owner_is_observed_as_closed_and_shutdown_is_clean() {
    let report = run(RunConfig {
        events: 2,
        call_timeout_ms: 2_000,
        program: vec![
            FakeOutboundProgram::Stop,
            FakeOutboundProgram::Deliver("unused".into()),
        ],
    })
    .expect("owner stop and bounded shutdown settle");

    for observed in report.replies {
        assert_eq!(
            observed,
            DriverReply::Reply(RelayReply::DeadLetter {
                reason: DeadLetterReason::Unavailable(BridgeUnavailable::BridgeClosed),
            })
        );
    }
    assert_eq!(report.stats.delivered, 0);
    assert_eq!(report.stats.transient, 0);
    assert_eq!(report.stats.dead_letter, 2);
}

#[test]
fn invalid_configuration_returns_before_runtime_startup() {
    let error = run(RunConfig {
        events: 1,
        call_timeout_ms: 2_000,
        program: vec![],
    })
    .expect_err("mismatched script must fail validation");

    assert!(matches!(
        error,
        RunError::InvalidConfig(RunConfigError::ProgramLengthMismatch {
            events: 1,
            entries: 0,
        })
    ));
}

#[test]
fn real_sqs_path_installs_on_the_same_facade_and_drains_before_shutdown() {
    for _ in 0..5 {
        let (endpoint, server) = one_shot_http_500();
        let report = run_against_sqs(
            1,
            2_000,
            SqsConfig::default()
                .with_endpoint_url(endpoint.clone())
                .with_credentials(SqsCredentials::new("test", "test"))
                .with_default_timeout(Duration::from_secs(1)),
            format!("{endpoint}/queue"),
            Duration::from_secs(1),
        )
        .expect("typed SQS failure is a relay reply, not a runner failure");
        server.join().expect("fake SQS server");

        assert!(matches!(
            report.workload.replies.as_slice(),
            [DriverReply::Reply(RelayReply::DeadLetter {
                reason: DeadLetterReason::Fatal(BridgeFatal::SdkUnknown),
            })]
        ));
        assert_eq!(report.workload.stats.dead_letter, 1);
        assert!(report.drain.closed);
        assert!(report.drain.drained);
        assert_eq!(report.drain.in_flight_remaining, 0);
    }
}

#[test]
fn real_sqs_path_preserves_typed_install_failure_through_clean_shutdown() {
    let error = run_against_sqs(
        1,
        1_000,
        SqsConfig::default().with_mailbox_capacity(0),
        "http://127.0.0.1/queue".into(),
        Duration::from_secs(1),
    )
    .expect_err("invalid bridge config must remain typed");

    let RunError::Terminal(terminal) = error else {
        panic!("expected terminal workload error, got {error:?}");
    };
    match terminal.as_ref() {
        RunToShutdownError::Workload(report) => assert!(matches!(
            report.get_ref(),
            RelayWorkloadError::BridgeInstall(SqsInstallError::Config(
                SqsConfigError::ZeroMailboxCapacity
            ))
        )),
        other => panic!("expected clean shutdown after install failure, got {other:?}"),
    }
}

#[test]
fn real_sqs_inputs_are_bounded_before_runtime_startup() {
    assert!(matches!(
        run_against_sqs(
            1,
            1_000,
            SqsConfig::default(),
            "   ".into(),
            Duration::from_secs(1),
        ),
        Err(RunError::InvalidConfig(RunConfigError::EmptyQueueUrl))
    ));
    assert!(matches!(
        run_against_sqs(
            1,
            1_000,
            SqsConfig::default(),
            "http://127.0.0.1/queue".into(),
            Duration::from_secs(60) + Duration::from_nanos(1),
        ),
        Err(RunError::InvalidConfig(
            RunConfigError::DurationTooLarge {
                field: "bridge timeout",
                requested_ms: 60_001,
                max_ms: 60_000,
            }
        ))
    ));
}
