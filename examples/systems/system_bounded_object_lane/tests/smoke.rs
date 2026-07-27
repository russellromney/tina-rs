use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use system_bounded_object_lane::{
    RunConfig, S3RunError, S3WorkloadError, WorkFailure, run, run_against_s3, run_put_terminals,
};
use tina_aws_bridge::{InstallError, S3Config, S3ConfigError, S3Credentials};
use tina_runtime::RunToShutdownError;

fn one_shot_http_500() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake S3");
    listener
        .set_nonblocking(true)
        .expect("make fake S3 accept bounded");
    let address = listener.local_addr().expect("fake S3 address");
    let thread = std::thread::spawn(move || {
        // Generous accept/read bounds: the typed-failure proof must not
        // race the bridge's own deadlines under CI load.
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept fake S3 request: {error}"),
            }
        };
        // Accepted sockets inherit O_NONBLOCK from the listener on
        // macOS/BSD (Linux clears it), which turns the first read into an
        // instant WouldBlock. Restore blocking mode on both platforms.
        stream
            .set_nonblocking(false)
            .expect("fake S3 stream blocking");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set fake S3 timeout");
        let mut request = [0_u8; 8 * 1024];
        let _ = stream.read(&mut request).expect("read fake S3 request");
        stream
            .write_all(
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .expect("write fake S3 response");
    });
    (format!("http://{address}"), thread)
}

#[test]
fn overload_is_visible_as_busy_not_hidden_queueing() {
    let report = run(RunConfig {
        callers: 10,
        lane_in_flight: 2,
        lane_mailbox: 32,
        work_ms: 100,
        call_timeout_ms: 2_000,
    })
    .expect("run succeeds");

    assert_eq!(report.callers, 10);
    assert_eq!(report.failed, 0);
    assert!(report.work_failures.is_empty());
    assert_eq!(report.stored, 2);
    assert_eq!(report.busy, 8);
    assert_eq!(report.stats.accepted, 2);
    assert_eq!(report.stats.work_completed, 2);
    assert_eq!(report.stats.completed, 2);
    assert_eq!(report.stats.busy, 8);
    assert_eq!(report.stats.current, 0);
    assert_eq!(report.stats.retired, 0);
    assert_eq!(report.stats.caller_gone, 0);
    assert!(report.stats.counts_agree);
    assert!(report.stats.settlements_agree);
    assert_eq!(report.dropped_permits, 0);
    assert_eq!(report.full, 0);
    assert_eq!(report.closed, 0);
    assert_eq!(report.timeout, 0);
    assert_eq!(report.rejected, 0);
    assert!(report.rejection_reasons.is_empty());
}

#[test]
fn report_failure_returns_after_bounded_shutdown() {
    let error = run(RunConfig {
        lane_mailbox: 0,
        ..RunConfig::default()
    })
    .expect_err("zero-capacity mailbox must refuse the stats call");

    assert!(
        format!("{error:#}").contains("stats call mailbox was full"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn host_mailbox_full_is_an_exact_put_terminal() {
    // With a zero mailbox the Put never enters the isolate. Full must remain
    // distinct from Busy (admission pressure) and from stats-path failure.
    let report = run_put_terminals(RunConfig {
        callers: 4,
        lane_in_flight: 2,
        lane_mailbox: 0,
        work_ms: 1,
        call_timeout_ms: 200,
    })
    .expect("put terminals must remain observable");

    assert_eq!(report.callers, 4);
    assert_eq!(
        report.full, 4,
        "zero mailbox must Full every Put: {report:?}"
    );
    assert_eq!(
        report.busy, 0,
        "mailbox Full must not become Busy: {report:?}"
    );
    assert_eq!(report.stored, 0, "report={report:?}");
    assert_eq!(report.closed, 0, "report={report:?}");
    assert_eq!(report.timeout, 0, "report={report:?}");
    assert_eq!(report.rejected, 0, "report={report:?}");
    // Stats were not observed on this path — agreement flags stay false.
    assert!(!report.stats.counts_agree, "report={report:?}");
    assert!(!report.stats.settlements_agree, "report={report:?}");
}

#[test]
fn invalid_config_returns_without_allocating_or_panicking() {
    let error = run(RunConfig {
        callers: 0,
        ..RunConfig::default()
    })
    .expect_err("zero callers is unsafe configuration");
    assert!(format!("{error:#}").contains("callers=0 is outside 1..=10000"));

    let error = run(RunConfig {
        lane_mailbox: 100_001,
        ..RunConfig::default()
    })
    .expect_err("oversized mailbox is unsafe configuration");
    assert!(format!("{error:#}").contains("lane_mailbox=100001"));

    let error = run(RunConfig {
        work_ms: 60_001,
        ..RunConfig::default()
    })
    .expect_err("oversized work duration is unsafe configuration");
    assert!(format!("{error:#}").contains("work_ms=60001ms exceeds maximum 60000ms"));

    let error = run(RunConfig {
        call_timeout_ms: 60_001,
        ..RunConfig::default()
    })
    .expect_err("oversized caller timeout is unsafe configuration");
    assert!(format!("{error:#}").contains("call_timeout_ms=60001ms exceeds maximum 60000ms"));
}

#[test]
fn real_s3_path_installs_on_the_same_facade_and_drains_before_shutdown() {
    for _ in 0..5 {
        let (endpoint, server) = one_shot_http_500();
        let report = run_against_s3(
            RunConfig {
                callers: 1,
                lane_in_flight: 1,
                lane_mailbox: 8,
                work_ms: 0,
                call_timeout_ms: 2_000,
            },
            S3Config::default()
                .with_endpoint_url(endpoint)
                .with_force_path_style(true)
                .with_credentials(S3Credentials::new("test", "test"))
                .with_default_timeout(Duration::from_secs(5)),
            "bucket".into(),
            "prefix/".into(),
            Duration::from_secs(5),
        )
        .expect("typed S3 failure is an application reply, not a runner failure");
        server.join().expect("fake S3 server");

        assert_eq!(report.workload.callers, 1);
        assert_eq!(report.workload.failed, 1);
        assert!(matches!(
            report.workload.work_failures.as_slice(),
            [WorkFailure::S3(tina_aws_bridge::S3Error::Sdk(_))]
        ));
        assert_eq!(report.workload.stats.current, 0);
        assert!(report.workload.stats.settlements_agree);
        assert!(report.drain.closed);
        assert!(report.drain.drained);
        assert_eq!(report.drain.in_flight_remaining, 0);
    }
}

#[test]
fn real_s3_path_preserves_typed_install_failure_through_clean_shutdown() {
    let error = run_against_s3(
        RunConfig::default(),
        S3Config::default().with_mailbox_capacity(0),
        "bucket".into(),
        String::new(),
        Duration::from_secs(1),
    )
    .expect_err("invalid bridge config must remain typed");

    let S3RunError::Terminal(terminal) = error else {
        panic!("expected terminal workload error, got {error:?}");
    };
    match terminal.as_ref() {
        RunToShutdownError::Workload(report) => assert!(matches!(
            report.get_ref(),
            S3WorkloadError::Install(InstallError::Config(S3ConfigError::ZeroMailboxCapacity))
        )),
        other => panic!("expected clean shutdown after install failure, got {other:?}"),
    }
}

#[test]
fn real_s3_inputs_are_bounded_before_runtime_startup() {
    assert!(matches!(
        run_against_s3(
            RunConfig::default(),
            S3Config::default(),
            "   ".into(),
            String::new(),
            Duration::from_secs(1),
        ),
        Err(S3RunError::InvalidConfig(
            system_bounded_object_lane::RunConfigError::EmptyS3Bucket
        ))
    ));
    assert!(matches!(
        run_against_s3(
            RunConfig::default(),
            S3Config::default(),
            "bucket".into(),
            String::new(),
            Duration::from_secs(60) + Duration::from_nanos(1),
        ),
        Err(S3RunError::InvalidConfig(
            system_bounded_object_lane::RunConfigError::DurationTooLarge {
                field: "bridge_timeout",
                value_ms: 60_001,
                max_ms: 60_000,
            }
        ))
    ));
}
