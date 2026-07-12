use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use tina::prelude::*;
use tina_runtime::{DefaultThreadedMailboxFactory, LocalSystem, LocalSystemState, ResultWaitError};

#[derive(Debug)]
enum ResultMsg {
    Finish(u64),
    Stop,
}

struct ResultWorker;

#[tina_runtime::isolate(message = ResultMsg)]
impl ResultWorker {
    fn handle(
        &mut self,
        message: ResultMsg,
        _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            ResultMsg::Finish(value) => stop_with(value),
            ResultMsg::Stop => stop(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug)]
enum MultiResultMsg {
    Finish(u64),
}

struct MultiResultWorker;

#[tina_runtime::isolate(message = MultiResultMsg, shard = AppShard)]
impl MultiResultWorker {
    fn handle(
        &mut self,
        message: MultiResultMsg,
        _ctx: &mut Context<'_, AppShard, Self::Reply>,
    ) -> Effect<Self> {
        match message {
            MultiResultMsg::Finish(value) => stop_with(value),
        }
    }
}

#[test]
fn local_system_observe_result_preserves_typed_stop_with_value() {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system");
    let worker = app
        .register_root::<ResultWorker, Infallible>(ResultWorker, 4)
        .expect("register result worker");

    let waiter = app
        .observe_result::<u64, _, _>(worker)
        .expect("register waiter before trigger");
    app.try_send(worker, ResultMsg::Finish(42))
        .expect("trigger result");

    assert_eq!(waiter.wait(Duration::from_secs(1)), Ok(42));
    assert_eq!(
        app.observe_result::<u64, _, _>(worker).err(),
        Some(ResultWaitError::AlreadyStopped),
        "late observation must preserve the eager stopped error"
    );
    let terminal = app.shutdown().drain().join_report();
    terminal.ensure_clean().expect("clean shutdown");
}

#[test]
fn local_system_observe_result_preserves_stopped_without_result() {
    let app = LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
        .try_build()
        .expect("start local system");
    let worker = app
        .register_root::<ResultWorker, Infallible>(ResultWorker, 4)
        .expect("register result worker");

    let waiter = app
        .observe_result::<u64, _, _>(worker)
        .expect("register waiter before trigger");
    app.try_send(worker, ResultMsg::Stop)
        .expect("stop without result");

    assert_eq!(
        waiter.wait(Duration::from_secs(1)),
        Err(ResultWaitError::StoppedWithoutResult)
    );
    let terminal = app.shutdown().drain().join_report();
    terminal.ensure_clean().expect("clean shutdown");
}

#[test]
fn shared_local_system_shutdown_handle_is_cached_and_idempotent() {
    let app = Arc::new(
        LocalSystem::single_shard(SingleShard, DefaultThreadedMailboxFactory)
            .try_build()
            .expect("start local system"),
    );
    let clone_held_by_host = Arc::clone(&app);
    let handle = app.shutdown_handle();

    let first = handle
        .request_and_wait_report(Duration::from_secs(2))
        .expect("request and wait");
    first.ensure_clean().expect("clean terminal report");
    let second = handle
        .request_and_wait_report(Duration::ZERO)
        .expect("cached report needs no wait budget");

    assert_eq!(first.state(), LocalSystemState::Closed);
    assert_eq!(first.state(), second.state());
    assert_eq!(first.error(), second.error());
    assert_eq!(first.shutdown_report(), second.shutdown_report());
    assert_eq!(Arc::strong_count(&app), 2);
    drop(clone_held_by_host);
    drop(app);
}

#[test]
fn local_multi_shard_observe_result_routes_to_address_owner() {
    let app = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(AppShard(10))
        .shard(AppShard(20))
        .try_build()
        .expect("start multi-shard local system");
    let worker = app
        .register_root_on::<MultiResultWorker, Infallible>(ShardId::new(20), MultiResultWorker, 4)
        .expect("register on second shard");

    let waiter = app
        .observe_result::<u64, _, _>(worker)
        .expect("register waiter on owning shard");
    app.try_send(worker, MultiResultMsg::Finish(99))
        .expect("trigger result");

    assert_eq!(waiter.wait(Duration::from_secs(1)), Ok(99));
    let terminal = app.shutdown().drain().join_report();
    terminal.ensure_clean().expect("clean shutdown");
}

#[test]
#[should_panic(expected = "unknown shard")]
fn local_multi_shard_observe_result_rejects_foreign_shard() {
    let owner = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(AppShard(30))
        .try_build()
        .expect("start owner");
    let worker = owner
        .register_root_on::<MultiResultWorker, Infallible>(ShardId::new(30), MultiResultWorker, 4)
        .expect("register worker");
    let foreign = LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
        .shard(AppShard(40))
        .try_build()
        .expect("start foreign owner");

    let _ = foreign.observe_result::<u64, _, _>(worker);
}

#[test]
fn shared_local_multi_shard_shutdown_handle_reports_every_shard() {
    let app = Arc::new(
        LocalSystem::multi_shard(DefaultThreadedMailboxFactory)
            .shard(AppShard(50))
            .shard(AppShard(60))
            .try_build()
            .expect("start multi-shard local system"),
    );
    let clone_held_by_host = Arc::clone(&app);
    let report = app
        .shutdown_handle()
        .request_and_wait_report(Duration::from_secs(2))
        .expect("multi-shard terminal report");

    report.ensure_clean().expect("clean multi-shard shutdown");
    assert_eq!(report.state(), LocalSystemState::Closed);
    assert_eq!(
        report.topology().expect("terminal topology").shards().len(),
        2
    );
    assert_eq!(Arc::strong_count(&app), 2);
    drop(clone_held_by_host);
    drop(app);
}
