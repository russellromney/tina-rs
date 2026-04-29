use super::*;
use tina::RestartPolicy;

#[test]
fn supervised_one_for_one_restarts_only_failed_child() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
        runtime.try_send(lineage_address(first_child), LineageMsg::Panic),
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
        runtime.try_send(lineage_address(first_child), LineageMsg::SpawnChild),
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
fn supervised_one_for_all_restarts_every_direct_restartable_child() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
        runtime.try_send(lineage_address(first_child), LineageMsg::Panic),
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
        runtime.try_send(lineage_address(second_child), LineageMsg::Panic),
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
    let root = runtime.register(new_root(), root_mailbox());
    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(3)),
    );

    assert_eq!(runtime.try_send(root, LineageMsg::SpawnChild), Ok(()));
    assert_eq!(runtime.step(), 1);
    let child = last_spawned_child(runtime.trace());

    assert_eq!(
        runtime.try_send(lineage_address(child), LineageMsg::Panic),
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
        runtime.try_send(lineage_address(child), LineageMsg::Panic),
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
fn stopped_supervisor_rejects_later_child_failure_without_replacement() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
        runtime.try_send(lineage_address(child), LineageMsg::Panic),
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
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
        runtime.try_send(lineage_address(panicking_child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(
        runtime.try_send(lineage_address(stopping_child), LineageMsg::Stop),
        Ok(())
    );
    assert_eq!(runtime.step(), 2);

    assert_eq!(factory_calls.get(), 2);
    assert_eq!(supervisor_events(runtime.trace()), Vec::new());
}

#[test]
fn supervise_before_children_and_reconfigure_reset_budget_are_supported() {
    let factory_calls = Rc::new(Cell::new(0));
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
        runtime.try_send(lineage_address(child), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);
    let replacement = runtime.child_record_snapshot()[0].child_isolate;

    runtime.supervise(
        root,
        SupervisorConfig::new(RestartPolicy::OneForOne, tina::RestartBudget::new(1)),
    );
    assert_eq!(
        runtime.try_send(lineage_address(replacement), LineageMsg::Panic),
        Ok(())
    );
    assert_eq!(runtime.step(), 1);

    assert_eq!(factory_calls.get(), 3);
    assert_eq!(runtime.supervisor_snapshot().len(), 1);
}

#[test]
fn supervise_panics_for_unknown_stale_or_cross_shard_parent_addresses() {
    let mut runtime = CurrentRuntime::new(TestShard, TestMailboxFactory);
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
