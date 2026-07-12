use super::*;
use tina::RestartPolicy;

#[derive(Debug)]
struct PanickingRestartFactoryRoot {
    factory_calls: Rc<Cell<usize>>,
}

impl Isolate for PanickingRestartFactoryRoot {
    type Message = LineageMsg;
    type Reply = ();
    type Send = Outbound<NeverOutbound>;
    type Spawn = tina::RestartableChildDefinition<ChildIsolate>;
    type SpawnObserved = std::convert::Infallible;
    type Io = std::convert::Infallible;
    type Fact = ::std::convert::Infallible;
    type Shard = TestShard;

    fn handle(
        &mut self,
        msg: Self::Message,
        _ctx: &mut Context<'_, Self::Shard, Self::Reply>,
    ) -> Effect<Self> {
        match msg {
            LineageMsg::SpawnChild => {
                let factory_calls = Rc::clone(&self.factory_calls);
                Effect::Spawn(tina::RestartableChildDefinition::new(
                    move || {
                        let call = factory_calls.get();
                        factory_calls.set(call + 1);
                        assert_eq!(call, 0, "restart factory panicked");
                        ChildIsolate { leaf_capacity: 3 }
                    },
                    3,
                ))
            }
            LineageMsg::Stop => Effect::Stop,
            LineageMsg::Panic => panic!("panic inside panicking-factory root"),
            LineageMsg::Fail => Effect::Fail,
            LineageMsg::StopChildren => Effect::StopChildren,
            LineageMsg::Restart | LineageMsg::SpawnGrandchild => Effect::Noop,
        }
    }
}

#[test]
fn supervised_one_for_one_restarts_only_failed_child() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let first_child = last_spawned_child(runtime.trace());
    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let second_child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.try_send(lineage_address(&runtime, first_child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    let records = runtime.child_record_snapshot();
    assert_ne!(records[0].child_isolate, first_child);
    assert_eq!(records[0].child_ordinal, 0);
    assert_eq!(
        records[1],
        child_record(root.isolate(), second_child, 1, 3, true)
    );
    assert_eq!(factory_calls.get(), 3);
    assert_eq!(
        runtime.try_send(
            lineage_address(&runtime, first_child),
            LineageMsg::SpawnChild
        ),
        Err(TrySendError::Closed(LineageMsg::SpawnChild))
    );
    assert!(
        supervisor_events(runtime.trace())
            .iter()
            .any(|kind| matches!(
                kind,
                RuntimeEventKind::SupervisorRestartTriggered {
                    policy: RestartPolicy::OneForOne,
                    failed_child,
                    failed_ordinal: 0,
                } if *failed_child == first_child
            ))
    );

    let panic_id = runtime
        .trace()
        .iter()
        .find(|event| {
            event.isolate() == first_child
                && matches!(event.kind(), RuntimeEventKind::HandlerPanicked)
        })
        .expect("expected child panic event")
        .id();
    let direct_consequences: Vec<_> = runtime
        .trace()
        .iter()
        .filter(|event| event.cause() == Some(CauseId::new(panic_id)))
        .map(|event| event.kind())
        .collect();
    assert!(
        direct_consequences
            .iter()
            .any(|kind| matches!(kind, RuntimeEventKind::IsolateStopped))
    );
    assert!(direct_consequences.iter().any(|kind| {
        matches!(
            kind,
            RuntimeEventKind::SupervisorRestartTriggered {
                failed_child,
                ..
            } if *failed_child == first_child
        )
    }));
}

#[test]
fn supervised_restart_factory_panic_is_contained_and_skipped() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        PanickingRestartFactoryRoot {
            factory_calls: Rc::clone(&factory_calls),
        },
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::Panic),
        Ok(())
    );
    let step_result = catch_unwind(AssertUnwindSafe(|| runtime.step()));
    assert!(
        step_result.is_ok(),
        "restart factory panic must not escape the runtime step"
    );

    assert_eq!(factory_calls.get(), 2, "initial spawn + failed restart");
    assert!(
        runtime.trace().iter().any(|event| matches!(
            event.kind(),
            RuntimeEventKind::RestartChildSkipped {
                old_isolate,
                reason: RestartSkippedReason::FactoryPanicked,
                ..
            } if old_isolate == child
        )),
        "factory panic must become a visible restart-skip event"
    );
    assert!(
        !runtime
            .trace()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::RestartChildCompleted { .. })),
        "failed factory must not report a completed restart"
    );
    assert_eq!(runtime.try_send(root, LineageMsg::Stop), Ok(()));
    assert_eq!(runtime.step(), 1, "supervisor shard remains usable");
}

#[test]
fn supervised_one_for_all_restarts_every_direct_restartable_child() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForAll, tina::RestartBudget::new(3)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let first_child = last_spawned_child(runtime.trace());
    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let second_child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.try_send(lineage_address(&runtime, first_child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    let records = runtime.child_record_snapshot();
    assert_ne!(records[0].child_isolate, first_child);
    assert_ne!(records[1].child_isolate, second_child);
    assert_eq!(records[0].child_ordinal, 0);
    assert_eq!(records[1].child_ordinal, 1);
    assert_eq!(factory_calls.get(), 4);
}

#[test]
fn supervised_rest_for_one_restarts_failed_and_younger_children_only() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::RestForOne, tina::RestartBudget::new(3)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let first_child = last_spawned_child(runtime.trace());
    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let second_child = last_spawned_child(runtime.trace());
    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let third_child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.try_send(lineage_address(&runtime, second_child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    let records = runtime.child_record_snapshot();
    assert_eq!(
        records[0],
        child_record(root.isolate(), first_child, 0, 3, true)
    );
    assert_ne!(records[1].child_isolate, second_child);
    assert_ne!(records[2].child_isolate, third_child);
    assert_eq!(records[1].child_ordinal, 1);
    assert_eq!(records[2].child_ordinal, 2);
    assert_eq!(factory_calls.get(), 5);
}

#[test]
fn supervised_restart_skips_selected_non_restartable_child() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    assert_eq!(
        runtime.child_record_snapshot(),
        vec![child_record(root.isolate(), child, 0, 2, false)]
    );
    assert!(
        restart_child_events(runtime.trace())
            .iter()
            .any(|kind| matches!(
                kind,
                RuntimeEventKind::RestartChildSkipped {
                    old_isolate,
                    reason: RestartSkippedReason::NotRestartable,
                    ..
                } if *old_isolate == child
            ))
    );
}

#[test]
fn supervised_restart_budget_exhaustion_is_visible_and_creates_no_replacement() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(0)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    assert_eq!(
        runtime.child_record_snapshot(),
        vec![child_record(root.isolate(), child, 0, 3, true)]
    );
    assert_eq!(factory_calls.get(), 1);
    assert!(
        supervisor_events(runtime.trace())
            .iter()
            .any(|kind| matches!(
                kind,
                RuntimeEventKind::SupervisorRestartRejected {
                    failed_child,
                    failed_ordinal: 0,
                    reason: SupervisionRejectedReason::BudgetExceeded {
                        attempted_restart: 1,
                        max_restarts: 0,
                    },
                } if *failed_child == child
            ))
    );
}

#[test]
fn windowed_restart_budget_resets_after_period() {
    let factory_calls = Rc::new(Cell::new(0));
    let (mut runtime, clock) = new_manual_runtime();
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(
            RestartPolicy::OneForOne,
            tina::RestartBudget::within(1, Duration::from_secs(10)),
        ),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let first_child = last_spawned_child(runtime.trace());
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, first_child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);
    let first_replacement = runtime.child_record_snapshot()[0].child_isolate;

    clock.advance(Duration::from_secs(10));
    assert_eq!(
        runtime.try_send(
            lineage_address(&runtime, first_replacement),
            LineageMsg::Panic
        ),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);
    let second_replacement = runtime.child_record_snapshot()[0].child_isolate;

    assert_ne!(first_replacement, second_replacement);
    assert_eq!(
        runtime.entry_count(),
        2,
        "old stopped child entries should be collected after restart work settles"
    );
    assert_eq!(factory_calls.get(), 3);
    assert!(
        !supervisor_events(runtime.trace())
            .iter()
            .any(|kind| matches!(
                kind,
                RuntimeEventKind::SupervisorRestartRejected {
                    reason: SupervisionRejectedReason::BudgetExceeded { .. },
                    ..
                }
            )),
        "windowed budget should reset after the configured period"
    );
}

#[test]
fn stopped_supervisor_rejects_later_child_failure_without_replacement() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());
    assert_eq!(runtime.try_send(root, LineageMsg::Stop), Ok(()));
    assert_eq!(runtime.step(), 1);

    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    assert_eq!(factory_calls.get(), 1);
    assert!(
        supervisor_events(runtime.trace())
            .iter()
            .any(|kind| matches!(
                kind,
                RuntimeEventKind::SupervisorRestartRejected {
                    failed_child,
                    failed_ordinal: 0,
                    reason: SupervisionRejectedReason::SupervisorStopped,
                } if *failed_child == child
            ))
    );
}

#[test]
fn unsupervised_child_panic_and_normal_child_stop_do_not_trigger_supervision() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let panicking_child = last_spawned_child(runtime.trace());
    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let stopping_child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.try_send(
            lineage_address(&runtime, panicking_child),
            LineageMsg::Panic
        ),
        Ok(())
    );
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, stopping_child), LineageMsg::Stop),
        Ok(())
    );
    assert_eq!(runtime.step(), 2);

    assert_eq!(factory_calls.get(), 2);
    assert_eq!(supervisor_events(runtime.trace()), Vec::new());
}

#[test]
fn supervise_before_children_and_reconfigure_reset_budget_are_supported() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );

    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(1)),
    );
    assert_eq!(runtime.supervisor_snapshot().len(), 1);

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);
    let replacement = runtime.child_record_snapshot()[0].child_isolate;

    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(1)),
    );
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, replacement), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    assert_eq!(factory_calls.get(), 3);
    assert_eq!(runtime.supervisor_snapshot().len(), 1);
}

#[test]
fn supervisor_report_summarizes_restart_history_for_one_owner() {
    use crate::supervision_report::SupervisorReport;

    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    let report = SupervisorReport::from_events(runtime.trace().iter(), root.isolate());
    assert_eq!(report.parent, root.isolate());
    assert_eq!(report.children_spawned, 1);
    assert_eq!(report.restarts_triggered, 1);
    assert_eq!(report.restarts_attempted, 1);
    assert_eq!(report.restarts_completed, 1);
    assert_eq!(report.skipped_not_restartable, 0);
    assert_eq!(report.rejected_budget_exceeded, 0);
    assert!(report.halt.is_none());
    assert!(report.non_zero());
    assert!(!report.halted());

    assert_eq!(report.children.len(), 1);
    let entry = report.children[0];
    assert_eq!(entry.child_ordinal, 0);
    assert_eq!(entry.restarts_completed, 1);
    // The report's latest incarnation matches the live record's replacement.
    let replacement = runtime.child_record_snapshot()[0].child_isolate;
    assert_eq!(entry.latest_isolate, replacement);
    assert_ne!(entry.latest_isolate, child);
}

#[test]
fn supervisor_report_names_budget_exhaustion_as_terminal_halt() {
    use crate::supervision_report::{SupervisorHalt, SupervisorReport};

    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(0)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    let report = SupervisorReport::from_events(runtime.trace().iter(), root.isolate());
    assert_eq!(report.children_spawned, 1);
    assert_eq!(report.restarts_completed, 0);
    assert_eq!(report.rejected_budget_exceeded, 1);
    assert!(report.halted());
    assert_eq!(
        report.halt,
        Some(SupervisorHalt::BudgetExhausted {
            attempted_restart: 1,
            max_restarts: 0,
        })
    );
}

#[test]
fn non_panic_child_failure_restarts_with_new_incarnation_and_rejects_stale_address() {
    use crate::supervision_report::SupervisorReport;

    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    // The child fails as a typed, non-panic outcome.
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::Fail),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    // A fresh incarnation replaced the failed child (initial spawn + restart).
    let replacement = runtime.child_record_snapshot()[0].child_isolate;
    assert_ne!(replacement, child);
    assert_eq!(factory_calls.get(), 2);

    // The failure is recorded distinctly from a panic.
    assert!(
        runtime.trace().iter().any(|event| event.isolate() == child
            && matches!(event.kind(), RuntimeEventKind::HandlerReportedFailure)),
        "typed failure must record HandlerReportedFailure"
    );
    assert!(
        !runtime
            .trace()
            .iter()
            .any(|event| matches!(event.kind(), RuntimeEventKind::HandlerPanicked)),
        "a typed failure must not be recorded as a panic"
    );

    // The stale pre-restart address rejects loudly.
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::SpawnChild),
        Err(TrySendError::Closed(LineageMsg::SpawnChild))
    );

    // The same supervision path ran: one completed restart, naming the new
    // incarnation under the owner.
    let report = SupervisorReport::from_events(runtime.trace().iter(), root.isolate());
    assert_eq!(report.restarts_completed, 1);
    assert_eq!(report.children.len(), 1);
    assert_eq!(report.children[0].latest_isolate, replacement);
}

#[test]
fn stop_children_closes_owned_children_and_reports_them_without_stopping_owner() {
    use crate::supervision_report::SupervisorReport;

    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let first_child = last_spawned_child(runtime.trace());
    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let second_child = last_spawned_child(runtime.trace());

    // Both children accept messages before shutdown.
    assert_eq!(
        runtime.try_send(
            lineage_address(&runtime, first_child),
            LineageMsg::SpawnChild
        ),
        Ok(())
    );
    assert_eq!(
        runtime.try_send(
            lineage_address(&runtime, second_child),
            LineageMsg::SpawnChild
        ),
        Ok(())
    );

    // The owner stops its children as an explicit supervised shutdown.
    assert_eq!(runtime.try_send(root, LineageMsg::StopChildren), Ok(()));
    assert_eq!(runtime.step(), 1);

    // Both children are now closed; their pending messages were abandoned.
    assert_eq!(
        runtime.try_send(
            lineage_address(&runtime, first_child),
            LineageMsg::SpawnChild
        ),
        Err(TrySendError::Closed(LineageMsg::SpawnChild))
    );
    assert_eq!(
        runtime.try_send(
            lineage_address(&runtime, second_child),
            LineageMsg::SpawnChild
        ),
        Err(TrySendError::Closed(LineageMsg::SpawnChild))
    );

    // The owner itself stayed up: plain Stop never cascades, and neither does
    // StopChildren cascade to the owner.
    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));

    // The report names every child the owner closed, under the owner.
    let report = SupervisorReport::from_events(runtime.trace().iter(), root.isolate());
    assert_eq!(report.children_stopped, 2);
    let stopped: Vec<_> = report
        .children
        .iter()
        .filter(|child| child.stopped_by_owner)
        .map(|child| child.latest_isolate)
        .collect();
    assert_eq!(stopped.len(), 2);
    assert!(stopped.contains(&first_child));
    assert!(stopped.contains(&second_child));
}

#[test]
fn unsupervised_isolate_failure_stops_without_restart() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(
        new_restartable_root(Rc::clone(&factory_calls)),
        root_mailbox(),
    );
    // No `runtime.supervise(root, ...)`: the child has no supervisor.

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::Fail),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    // No replacement: the factory ran once, for the initial spawn.
    assert_eq!(factory_calls.get(), 1);
    // The failure and stop are recorded, but supervision did nothing.
    assert!(runtime.trace().iter().any(|event| event.isolate() == child
        && matches!(event.kind(), RuntimeEventKind::HandlerReportedFailure)),);
    assert_eq!(supervisor_events(runtime.trace()), Vec::new());
    assert_eq!(
        runtime.try_send(lineage_address(&runtime, child), LineageMsg::SpawnChild),
        Err(TrySendError::Closed(LineageMsg::SpawnChild))
    );
}

#[test]
fn supervise_panics_for_unknown_stale_or_cross_shard_parent_addresses() {
    let mut runtime = Runtime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());
    let config = SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(1));

    let unknown = Address::<LineageMsg>::new(ShardId::new(3), IsolateId::new(99));
    assert!(catch_unwind(AssertUnwindSafe(|| runtime.supervise(unknown, config))).is_err());

    let stale = Address::<LineageMsg>::new_with_generation(
        root.shard(),
        root.isolate(),
        AddressGeneration::new(99),
    );
    assert!(catch_unwind(AssertUnwindSafe(|| runtime.supervise(stale, config))).is_err());

    let cross_shard = Address::<LineageMsg>::new(ShardId::new(99), root.isolate());
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            runtime.supervise(cross_shard, config)
        }))
        .is_err()
    );
}
