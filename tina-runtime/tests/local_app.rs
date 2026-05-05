use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tina::{Mailbox, TrySendError, prelude::*};
use tina_runtime::{
    BetelgeuseBackedControlError, CallError, FileId, LocalApp, LocalAppState, MailboxFactory,
    RuntimeEventKind, TraceRetention, file_close, file_create, file_fsync, file_read, file_size,
    file_write, sleep,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct AppShard(u32);

impl Shard for AppShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

struct AppMailbox<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
    closed: Mutex<bool>,
}

impl<T> AppMailbox<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: Mutex::new(VecDeque::new()),
            closed: Mutex::new(false),
        }
    }
}

impl<T> Mailbox<T> for AppMailbox<T> {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_send(&self, message: T) -> Result<(), TrySendError<T>> {
        if *self.closed.lock().expect("closed lock") {
            return Err(TrySendError::Closed(message));
        }

        let mut queue = self.queue.lock().expect("queue lock");
        if queue.len() >= self.capacity {
            return Err(TrySendError::Full(message));
        }

        queue.push_back(message);
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        self.queue.lock().expect("queue lock").pop_front()
    }

    fn close(&self) {
        *self.closed.lock().expect("closed lock") = true;
    }
}

#[derive(Debug, Clone, Copy)]
struct AppMailboxFactory;

impl MailboxFactory for AppMailboxFactory {
    fn create<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        Box::new(AppMailbox::new(capacity))
    }
}

#[derive(Debug, Clone, Copy)]
struct PanickingMailboxFactory;

impl MailboxFactory for PanickingMailboxFactory {
    fn create<T: 'static>(&self, _capacity: usize) -> Box<dyn Mailbox<T>> {
        panic!("test mailbox factory panic")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LlamaMsg {
    Feed(u64),
}

#[derive(Debug)]
struct LlamaService {
    seen: Arc<Mutex<Vec<u64>>>,
}

#[tina_runtime::isolate(message = LlamaMsg, shard = AppShard)]
impl LlamaService {
    fn handle(&mut self, msg: LlamaMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            LlamaMsg::Feed(value) => {
                self.seen.lock().expect("seen lock").push(value);
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerMsg {
    Start,
    Finished(Result<(), CallError>),
}

#[derive(Debug)]
struct TimerService {
    seen: Arc<Mutex<Vec<&'static str>>>,
}

#[tina_runtime::isolate(message = TimerMsg, shard = AppShard)]
impl TimerService {
    fn handle(&mut self, msg: TimerMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            TimerMsg::Start => sleep(Duration::from_millis(1)).reply(TimerMsg::Finished),
            TimerMsg::Finished(Ok(())) => {
                self.seen.lock().expect("timer seen lock").push("finished");
                noop()
            }
            TimerMsg::Finished(Err(_)) => {
                self.seen.lock().expect("timer seen lock").push("failed");
                noop()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileMsg {
    Start(PathBuf),
    Opened(Result<FileId, CallError>, PathBuf),
    Wrote(Result<usize, CallError>, PathBuf, FileId),
    Synced(Result<(), CallError>, PathBuf, FileId),
    Sized(Result<u64, CallError>, PathBuf, FileId),
    Read(Result<Vec<u8>, CallError>, PathBuf, FileId),
    Closed(Result<(), CallError>, PathBuf, Vec<u8>),
}

#[derive(Debug)]
struct FileService {
    seen: Arc<Mutex<Vec<String>>>,
}

#[tina_runtime::isolate(message = FileMsg, shard = AppShard)]
impl FileService {
    fn handle(&mut self, msg: FileMsg, _ctx: &mut Context<'_, AppShard>) -> Effect<Self> {
        match msg {
            FileMsg::Start(path) => {
                file_create(path.clone()).reply(move |result| FileMsg::Opened(result, path))
            }
            FileMsg::Opened(Ok(file), path) => file_write(file, b"local app file".to_vec())
                .reply(move |result| FileMsg::Wrote(result, path, file)),
            FileMsg::Wrote(Ok(14), path, file) => {
                file_fsync(file).reply(move |result| FileMsg::Synced(result, path, file))
            }
            FileMsg::Synced(Ok(()), path, file) => {
                file_size(file).reply(move |result| FileMsg::Sized(result, path, file))
            }
            FileMsg::Sized(Ok(size), path, file) => file_read(file, size as usize)
                .reply(move |result| FileMsg::Read(result, path, file)),
            FileMsg::Read(Ok(bytes), path, file) => {
                file_close(file).reply(move |result| FileMsg::Closed(result, path, bytes))
            }
            FileMsg::Closed(Ok(()), path, bytes) => {
                let _ = fs::remove_file(path);
                self.seen
                    .lock()
                    .expect("file seen lock")
                    .push(String::from_utf8(bytes).expect("file payload utf8"));
                noop()
            }
            FileMsg::Opened(Err(_), _)
            | FileMsg::Wrote(Err(_), _, _)
            | FileMsg::Wrote(Ok(_), _, _)
            | FileMsg::Synced(Err(_), _, _)
            | FileMsg::Sized(Err(_), _, _)
            | FileMsg::Read(Err(_), _, _)
            | FileMsg::Closed(Err(_), _, _) => {
                self.seen
                    .lock()
                    .expect("file seen lock")
                    .push("failed".to_string());
                noop()
            }
        }
    }
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(predicate(), "condition did not become true before timeout");
}

#[test]
fn local_app_single_shard_is_canonical_live_owner() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalApp::single_shard(AppShard(34), AppMailboxFactory)
        .ingress_capacity(8)
        .trace_retention(TraceRetention::Bounded(64))
        .build();

    assert_eq!(app.state(), LocalAppState::Accepting);
    let address = app
        .register_root::<LlamaService, Infallible>(
            LlamaService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register root");

    app.try_send(address, LlamaMsg::Feed(7))
        .expect("bounded handoff");
    wait_until(|| seen.lock().expect("seen lock").as_slice() == [7]);

    let terminal = app.shutdown().drain().join().expect("local app shutdown");
    assert_eq!(terminal.state(), LocalAppState::Closed);
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::HandlerFinished {
                effect: tina_runtime::EffectKind::Noop
            }
        )
    }));
}

#[test]
fn local_app_multi_shard_uses_same_entry_name_for_topology() {
    let left_seen = Arc::new(Mutex::new(Vec::new()));
    let right_seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalApp::<AppShard, AppMailboxFactory>::multi_shard(AppMailboxFactory)
        .shard(AppShard(41))
        .shard(AppShard(42))
        .ingress_capacity(8)
        .shard_pair_capacity(8)
        .trace_retention(TraceRetention::Bounded(128))
        .build();

    let left = app
        .register_root_on::<LlamaService, Infallible>(
            ShardId::new(41),
            LlamaService {
                seen: Arc::clone(&left_seen),
            },
            8,
        )
        .expect("register left root");
    let right = app
        .register_root_on::<LlamaService, Infallible>(
            ShardId::new(42),
            LlamaService {
                seen: Arc::clone(&right_seen),
            },
            8,
        )
        .expect("register right root");

    app.try_send(left, LlamaMsg::Feed(1))
        .expect("left bounded handoff");
    app.try_send(right, LlamaMsg::Feed(2))
        .expect("right bounded handoff");

    wait_until(|| left_seen.lock().expect("left lock").as_slice() == [1]);
    wait_until(|| right_seen.lock().expect("right lock").as_slice() == [2]);

    let terminal = app
        .shutdown()
        .drain()
        .join()
        .expect("multi-shard local app shutdown");
    assert_eq!(terminal.state(), LocalAppState::Closed);
    assert!(terminal.trace().len() >= 2);
}

#[test]
fn llama_tcp_timer_service_uses_local_app_runtime_owned_time() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let app = LocalApp::single_shard(AppShard(50), AppMailboxFactory)
        .ingress_capacity(8)
        .trace_retention(TraceRetention::Bounded(128))
        .build();
    let address = app
        .register_root::<TimerService, Infallible>(
            TimerService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register timer root");

    app.try_send(address, TimerMsg::Start)
        .expect("timer start handoff");
    wait_until(|| seen.lock().expect("timer seen lock").as_slice() == ["finished"]);

    let terminal = app.shutdown().drain().join().expect("timer app shutdown");
    assert_eq!(terminal.state(), LocalAppState::Closed);
    assert!(terminal.trace().iter().any(|event| {
        matches!(
            event.kind(),
            RuntimeEventKind::CallCompleted {
                call_kind: tina_runtime::CallKind::Sleep,
                ..
            }
        )
    }));
}

#[test]
fn local_app_service_uses_runtime_owned_file_io() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "tina-local-app-file-{}-{unique}.txt",
        std::process::id()
    ));
    let app = LocalApp::single_shard(AppShard(51), AppMailboxFactory)
        .ingress_capacity(8)
        .trace_retention(TraceRetention::Bounded(256))
        .build();
    let address = app
        .register_root::<FileService, Infallible>(
            FileService {
                seen: Arc::clone(&seen),
            },
            8,
        )
        .expect("register file root");

    app.try_send(address, FileMsg::Start(path.clone()))
        .expect("file start handoff");
    wait_until(|| seen.lock().expect("file seen lock").as_slice() == ["local app file"]);

    let terminal = app.shutdown().drain().join().expect("file app shutdown");
    let _ = fs::remove_file(path);
    assert_eq!(terminal.state(), LocalAppState::Closed);
    for expected in [
        tina_runtime::CallKind::FileOpen,
        tina_runtime::CallKind::FileWriteAt,
        tina_runtime::CallKind::FileFsync,
        tina_runtime::CallKind::FileSize,
        tina_runtime::CallKind::FileReadAt,
        tina_runtime::CallKind::FileClose,
    ] {
        assert!(
            terminal.trace().iter().any(|event| {
                matches!(
                    event.kind(),
                    RuntimeEventKind::CallCompleted { call_kind, .. } if call_kind == expected
                )
            }),
            "expected {expected:?} completion in local app trace"
        );
    }
}

#[test]
fn local_app_join_report_reports_worker_failure_terminal_state() {
    let app = LocalApp::single_shard(AppShard(60), PanickingMailboxFactory)
        .ingress_capacity(8)
        .trace_retention(TraceRetention::Bounded(16))
        .build();

    assert_eq!(app.state(), LocalAppState::Accepting);
    assert_eq!(
        app.register_root::<LlamaService, Infallible>(
            LlamaService {
                seen: Arc::new(Mutex::new(Vec::new())),
            },
            8,
        ),
        Err(BetelgeuseBackedControlError::WorkerStopped)
    );

    let terminal = app.shutdown().drain().join_report();
    assert_eq!(terminal.state(), LocalAppState::Failed);
    assert_eq!(
        terminal.error(),
        Some(BetelgeuseBackedControlError::WorkerStopped)
    );
    assert!(terminal.trace().is_empty());
}
