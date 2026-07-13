use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tina::prelude::*;
use tina_aws_bridge::{
    DynamoConfig, DynamoConfigError, DynamoConsistency, DynamoError, DynamoGetItem,
    DynamoInstallError, DynamoMsg, DynamoRequest, DynamoWorker, InstallError, S3Config,
    S3ConfigError, S3Error, S3Msg, S3PutObject, S3Request, S3Worker, SecretsConfig,
    SecretsConfigError, SecretsError, SecretsGetSecretValue, SecretsInstallError, SecretsMsg,
    SecretsRequest, SecretsWorker, SnsConfig, SnsConfigError, SnsDestination, SnsError,
    SnsInstallError, SnsMsg, SnsPublish, SnsRequest, SnsWorker, SqsConfig, SqsConfigError,
    SqsError, SqsInstallError, SqsMsg, SqsRequest, SqsSendMessage, SqsWorker,
};
use tina_runtime::{CallOutcome, DefaultThreadedMailboxFactory, LocalSystem, ThreadedRuntimeError};

fn system() -> LocalSystem<SingleShard, DefaultThreadedMailboxFactory> {
    LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system")
}

fn s3_request() -> S3Request {
    S3Request::PutObject(S3PutObject {
        bucket: "bucket".into(),
        key: "key".into(),
        body: b"body".to_vec(),
        content_type: None,
    })
}

fn sqs_request() -> SqsRequest {
    SqsRequest::SendMessage(SqsSendMessage {
        queue_url: "http://127.0.0.1/queue".into(),
        body: "body".into(),
        message_group_id: None,
        message_deduplication_id: None,
    })
}

fn sns_request() -> SnsRequest {
    SnsRequest::Publish(SnsPublish {
        destination: SnsDestination::TopicArn("arn:aws:sns:us-east-1:1:test".into()),
        message: "body".into(),
        subject: None,
        message_group_id: None,
        message_deduplication_id: None,
        attributes: HashMap::new(),
    })
}

fn dynamo_request() -> DynamoRequest {
    DynamoRequest::GetItem(DynamoGetItem {
        table_name: "table".into(),
        key: HashMap::new(),
        consistency: DynamoConsistency::Eventual,
    })
}

fn secrets_request() -> SecretsRequest {
    SecretsRequest::GetSecretValue(SecretsGetSecretValue {
        secret_id: "secret".into(),
        version_id: None,
        version_stage: None,
    })
}

#[test]
fn local_installs_return_callable_handles_drain_while_alive_and_shutdown_cleanly() {
    let app = system();
    let s3 = tina_aws_bridge::install_s3_local(&app, S3Config::default()).expect("install S3");
    let sqs = tina_aws_bridge::install_sqs_local(&app, SqsConfig::default()).expect("install SQS");
    let sns = tina_aws_bridge::install_sns_local(&app, SnsConfig::default()).expect("install SNS");
    let dynamo = tina_aws_bridge::install_dynamodb_local(&app, DynamoConfig::default())
        .expect("install DynamoDB");
    let secrets = tina_aws_bridge::install_secrets_local(&app, SecretsConfig::default())
        .expect("install Secrets Manager");

    assert!(!s3.closer.is_closed());
    assert!(!sqs.closer.is_closed());
    assert!(!sns.closer.is_closed());
    assert!(!dynamo.closer.is_closed());
    assert!(!secrets.closer.is_closed());

    assert!(s3.closer.close_and_drain(Duration::ZERO).drained);
    assert!(sqs.closer.close_and_drain(Duration::ZERO).drained);
    assert!(sns.closer.close_and_drain(Duration::ZERO).drained);
    assert!(dynamo.closer.close_and_drain(Duration::ZERO).drained);
    assert!(secrets.closer.close_and_drain(Duration::ZERO).drained);

    assert!(matches!(
        app.call_blocking(
            s3.address,
            S3Msg::Send(s3_request()),
            Duration::from_secs(2)
        ),
        Ok(CallOutcome::Replied(Err(S3Error::Closed)))
    ));
    assert!(matches!(
        app.call_blocking(
            sqs.address,
            SqsMsg::Send(sqs_request()),
            Duration::from_secs(2)
        ),
        Ok(CallOutcome::Replied(Err(SqsError::Closed)))
    ));
    assert!(matches!(
        app.call_blocking(
            sns.address,
            SnsMsg::Send(sns_request()),
            Duration::from_secs(2)
        ),
        Ok(CallOutcome::Replied(Err(SnsError::Closed)))
    ));
    assert!(matches!(
        app.call_blocking(
            dynamo.address,
            DynamoMsg::Send(dynamo_request()),
            Duration::from_secs(2),
        ),
        Ok(CallOutcome::Replied(Err(DynamoError::Closed)))
    ));
    assert!(matches!(
        app.call_blocking(
            secrets.address,
            SecretsMsg::Send(secrets_request()),
            Duration::from_secs(2),
        ),
        Ok(CallOutcome::Replied(Err(SecretsError::Closed)))
    ));

    assert_eq!(s3.metrics.snapshot().closed, 1);
    assert_eq!(sqs.metrics.snapshot().closed, 1);
    assert_eq!(sns.metrics.snapshot().closed, 1);
    assert_eq!(dynamo.metrics.snapshot().closed, 1);
    assert_eq!(secrets.metrics.snapshot().closed, 1);

    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean LocalSystem shutdown");
}

#[test]
fn local_installs_preserve_typed_config_and_stopped_worker_errors() {
    let app = system();
    assert!(matches!(
        S3Worker::<SingleShard>::install_local(&app, S3Config::default().with_mailbox_capacity(0),),
        Err(InstallError::Config(S3ConfigError::ZeroMailboxCapacity))
    ));
    assert!(matches!(
        SqsWorker::<SingleShard>::install_local(
            &app,
            SqsConfig::default().with_mailbox_capacity(0),
        ),
        Err(SqsInstallError::Config(SqsConfigError::ZeroMailboxCapacity))
    ));
    assert!(matches!(
        SnsWorker::<SingleShard>::install_local(
            &app,
            SnsConfig::default().with_mailbox_capacity(0),
        ),
        Err(SnsInstallError::Config(SnsConfigError::ZeroMailboxCapacity))
    ));
    assert!(matches!(
        DynamoWorker::<SingleShard>::install_local(
            &app,
            DynamoConfig::default().with_mailbox_capacity(0),
        ),
        Err(DynamoInstallError::Config(
            DynamoConfigError::ZeroMailboxCapacity
        ))
    ));
    assert!(matches!(
        SecretsWorker::<SingleShard>::install_local(
            &app,
            SecretsConfig::default().with_mailbox_capacity(0),
        ),
        Err(SecretsInstallError::Config(
            SecretsConfigError::ZeroMailboxCapacity
        ))
    ));

    app.shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("stop local system")
        .ensure_clean()
        .expect("clean stopped report");

    assert!(matches!(
        S3Worker::<SingleShard>::install_local(&app, S3Config::default()),
        Err(InstallError::Register(ThreadedRuntimeError::WorkerStopped))
    ));
    assert!(matches!(
        SqsWorker::<SingleShard>::install_local(&app, SqsConfig::default()),
        Err(SqsInstallError::Register(
            ThreadedRuntimeError::WorkerStopped
        ))
    ));
    assert!(matches!(
        SnsWorker::<SingleShard>::install_local(&app, SnsConfig::default()),
        Err(SnsInstallError::Register(
            ThreadedRuntimeError::WorkerStopped
        ))
    ));
    assert!(matches!(
        DynamoWorker::<SingleShard>::install_local(&app, DynamoConfig::default()),
        Err(DynamoInstallError::Register(
            ThreadedRuntimeError::WorkerStopped
        ))
    ));
    assert!(matches!(
        SecretsWorker::<SingleShard>::install_local(&app, SecretsConfig::default()),
        Err(SecretsInstallError::Register(
            ThreadedRuntimeError::WorkerStopped
        ))
    ));
}

#[test]
fn install_errors_preserve_typed_sources_for_every_service() {
    macro_rules! assert_source {
        ($error:expr, $source:ty) => {
            assert!(
                $error
                    .source()
                    .and_then(|source| source.downcast_ref::<$source>())
                    .is_some(),
                "{} must retain a typed {} source",
                $error,
                stringify!($source),
            );
        };
    }

    let s3_config = InstallError::Config(S3ConfigError::ZeroMailboxCapacity);
    let s3_build = InstallError::Build(S3Error::Internal("build failed".into()));
    let s3_register = InstallError::Register(ThreadedRuntimeError::WorkerStopped);
    assert_source!(s3_config, S3ConfigError);
    assert_source!(s3_build, S3Error);
    assert_source!(s3_register, ThreadedRuntimeError);

    let sqs_config = SqsInstallError::Config(SqsConfigError::ZeroMailboxCapacity);
    let sqs_build = SqsInstallError::Build(SqsError::Internal("build failed".into()));
    let sqs_register = SqsInstallError::Register(ThreadedRuntimeError::WorkerStopped);
    assert_source!(sqs_config, SqsConfigError);
    assert_source!(sqs_build, SqsError);
    assert_source!(sqs_register, ThreadedRuntimeError);

    let sns_config = SnsInstallError::Config(SnsConfigError::ZeroMailboxCapacity);
    let sns_build = SnsInstallError::Build(SnsError::Internal("build failed".into()));
    let sns_register = SnsInstallError::Register(ThreadedRuntimeError::WorkerStopped);
    assert_source!(sns_config, SnsConfigError);
    assert_source!(sns_build, SnsError);
    assert_source!(sns_register, ThreadedRuntimeError);

    let dynamo_config = DynamoInstallError::Config(DynamoConfigError::ZeroMailboxCapacity);
    let dynamo_build = DynamoInstallError::Build(DynamoError::Internal("build failed".into()));
    let dynamo_register = DynamoInstallError::Register(ThreadedRuntimeError::WorkerStopped);
    assert_source!(dynamo_config, DynamoConfigError);
    assert_source!(dynamo_build, DynamoError);
    assert_source!(dynamo_register, ThreadedRuntimeError);

    let secrets_config = SecretsInstallError::Config(SecretsConfigError::ZeroMailboxCapacity);
    let secrets_build = SecretsInstallError::Build(SecretsError::Internal("build failed".into()));
    let secrets_register = SecretsInstallError::Register(ThreadedRuntimeError::WorkerStopped);
    assert_source!(secrets_config, SecretsConfigError);
    assert_source!(secrets_build, SecretsError);
    assert_source!(secrets_register, ThreadedRuntimeError);
}

#[derive(Debug)]
enum GateMsg {
    Hold,
}

struct Gate {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

struct ReleaseOnDrop(Arc<AtomicBool>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tina_runtime::isolate(message = GateMsg)]
impl Gate {
    fn handle(
        &mut self,
        _message: GateMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        self.entered.store(true, Ordering::Release);
        while !self.release.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        noop()
    }
}

fn retry_command_full<T, E>(
    mut install: impl FnMut() -> Result<T, E>,
    is_full: impl Fn(&E) -> bool,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match install() {
            Ok(value) => return value,
            Err(error) if is_full(&error) && Instant::now() < deadline => {
                std::thread::yield_now();
            }
            Err(_) => panic!("registration did not refill before deadline"),
        }
    }
}

#[test]
fn local_installs_report_command_full_then_refill_for_every_service() {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .ingress_capacity(1)
        .try_build()
        .expect("start bounded local system");
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let release_on_drop = ReleaseOnDrop(Arc::clone(&release));
    let gate = app
        .register_root::<Gate, Infallible>(
            Gate {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            2,
        )
        .expect("register gate");
    app.try_send(gate, GateMsg::Hold).expect("occupy worker");
    let enter_deadline = Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::Acquire) && Instant::now() < enter_deadline {
        std::thread::yield_now();
    }
    assert!(entered.load(Ordering::Acquire), "gate did not enter");
    app.try_send(gate, GateMsg::Hold)
        .expect("fill host-control queue");

    assert!(matches!(
        S3Worker::<SingleShard>::install_local(&app, S3Config::default()),
        Err(InstallError::Register(ThreadedRuntimeError::CommandFull))
    ));
    assert!(matches!(
        SqsWorker::<SingleShard>::install_local(&app, SqsConfig::default()),
        Err(SqsInstallError::Register(ThreadedRuntimeError::CommandFull))
    ));
    assert!(matches!(
        SnsWorker::<SingleShard>::install_local(&app, SnsConfig::default()),
        Err(SnsInstallError::Register(ThreadedRuntimeError::CommandFull))
    ));
    assert!(matches!(
        DynamoWorker::<SingleShard>::install_local(&app, DynamoConfig::default()),
        Err(DynamoInstallError::Register(
            ThreadedRuntimeError::CommandFull
        ))
    ));
    assert!(matches!(
        SecretsWorker::<SingleShard>::install_local(&app, SecretsConfig::default()),
        Err(SecretsInstallError::Register(
            ThreadedRuntimeError::CommandFull
        ))
    ));

    release.store(true, Ordering::Release);
    drop(release_on_drop);
    let s3 = retry_command_full(
        || S3Worker::<SingleShard>::install_local(&app, S3Config::default()),
        |error| {
            matches!(
                error,
                InstallError::Register(ThreadedRuntimeError::CommandFull)
            )
        },
    );
    let sqs = retry_command_full(
        || SqsWorker::<SingleShard>::install_local(&app, SqsConfig::default()),
        |error| {
            matches!(
                error,
                SqsInstallError::Register(ThreadedRuntimeError::CommandFull)
            )
        },
    );
    let sns = retry_command_full(
        || SnsWorker::<SingleShard>::install_local(&app, SnsConfig::default()),
        |error| {
            matches!(
                error,
                SnsInstallError::Register(ThreadedRuntimeError::CommandFull)
            )
        },
    );
    let dynamo = retry_command_full(
        || DynamoWorker::<SingleShard>::install_local(&app, DynamoConfig::default()),
        |error| {
            matches!(
                error,
                DynamoInstallError::Register(ThreadedRuntimeError::CommandFull)
            )
        },
    );
    let secrets = retry_command_full(
        || SecretsWorker::<SingleShard>::install_local(&app, SecretsConfig::default()),
        |error| {
            matches!(
                error,
                SecretsInstallError::Register(ThreadedRuntimeError::CommandFull)
            )
        },
    );
    s3.closer.close();
    sqs.closer.close();
    sns.closer.close();
    dynamo.closer.close();
    secrets.closer.close();

    app.shutdown()
        .drain()
        .join_report()
        .ensure_clean()
        .expect("clean shutdown after refilled installs");
}
