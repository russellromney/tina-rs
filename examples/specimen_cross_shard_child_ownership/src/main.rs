use tina::{
    ChildDefinition, ChildRef, RestartBudget, RestartPolicy, SpawnObservedRemote, prelude::*,
};
use tina_runtime::{DefaultMailboxFactory, MultiShardRuntime};
use tina_supervisor::SupervisorConfig;

#[derive(Debug, Clone, Copy)]
struct DemoShard(u32);

impl Shard for DemoShard {
    fn id(&self) -> ShardId {
        ShardId::new(self.0)
    }
}

#[derive(Debug, Clone, Copy)]
enum WorkerMsg {
    Ping,
}

struct Worker;

#[tina_runtime::isolate(message = WorkerMsg, shard = DemoShard)]
impl Worker {
    fn handle(
        &mut self,
        msg: WorkerMsg,
        _ctx: &mut Context<'_, DemoShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            WorkerMsg::Ping => noop(),
        }
    }
}

#[derive(Debug)]
enum SupervisorMsg {
    Start,
    WorkerStarted(Result<ChildRef<WorkerMsg>, SpawnObservedError>),
    StopChildren,
}

struct Supervisor;

#[tina_runtime::isolate(
    message = SupervisorMsg,
    send = Outbound<WorkerMsg>,
    spawn_observed_remote = SpawnObservedRemote<ChildDefinition<Worker>, SupervisorMsg, WorkerMsg>,
    shard = DemoShard,
)]
impl Supervisor {
    fn handle(
        &mut self,
        msg: SupervisorMsg,
        _ctx: &mut Context<'_, DemoShard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            SupervisorMsg::Start => batch(vec![
                spawn_observed(ChildDefinition::new(Worker, 4))
                    .on_shard(ShardId::new(1))
                    .then(SupervisorMsg::WorkerStarted),
                spawn_observed(ChildDefinition::new(Worker, 4))
                    .on_shard(ShardId::new(2))
                    .then(SupervisorMsg::WorkerStarted),
            ]),
            SupervisorMsg::WorkerStarted(Ok(child)) => send(child.address, WorkerMsg::Ping),
            SupervisorMsg::WorkerStarted(Err(_)) => stop(),
            SupervisorMsg::StopChildren => stop_children(),
        }
    }
}

fn main() {
    let mut runtime = MultiShardRuntime::new([DemoShard(1), DemoShard(2)], DefaultMailboxFactory);
    let supervisor =
        runtime.register_with_capacity_on::<Supervisor, WorkerMsg>(ShardId::new(1), Supervisor, 8);
    runtime.supervise(
        supervisor,
        SupervisorConfig::new(RestartPolicy::OneForOne, RestartBudget::new(4)),
    );

    runtime.try_send(supervisor, SupervisorMsg::Start).unwrap();
    for _ in 0..12 {
        runtime.step();
    }

    let live = runtime.child_lifecycle_report(supervisor).unwrap();
    let shards: Vec<_> = live
        .children
        .iter()
        .map(|child| child.shard.get())
        .collect();
    println!(
        "child_lifecycle specimen=cross_shard_child_ownership parent={} children={} shards={:?} state=live",
        live.parent.get(),
        live.children.len(),
        shards
    );

    runtime
        .try_send(supervisor, SupervisorMsg::StopChildren)
        .unwrap();
    for _ in 0..12 {
        runtime.step();
    }

    let stopped = runtime.child_lifecycle_report(supervisor).unwrap();
    println!(
        "child_lifecycle specimen=cross_shard_child_ownership parent={} stopped={} pending_remote_control={}",
        stopped.parent.get(),
        stopped
            .children
            .iter()
            .filter(|child| matches!(child.state, tina_runtime::ChildLifecycleState::Stopped))
            .count(),
        stopped.pending_remote_control_count
    );
}
