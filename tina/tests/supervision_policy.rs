use tina::{ChildRelation, RestartBudget, RestartDecision, RestartPolicy};

#[test]
fn one_for_one_restarts_only_the_failed_child() {
    assert_eq!(
        RestartPolicy::OneForOne.decision(ChildRelation::BeforeFailed),
        RestartDecision::KeepRunning
    );
    assert_eq!(
        RestartPolicy::OneForOne.decision(ChildRelation::Failed),
        RestartDecision::Restart
    );
    assert_eq!(
        RestartPolicy::OneForOne.decision(ChildRelation::AfterFailed),
        RestartDecision::KeepRunning
    );
}

#[test]
fn one_for_all_restarts_every_child_in_the_group() {
    assert_eq!(
        RestartPolicy::OneForAll.decision(ChildRelation::BeforeFailed),
        RestartDecision::Restart
    );
    assert_eq!(
        RestartPolicy::OneForAll.decision(ChildRelation::Failed),
        RestartDecision::Restart
    );
    assert_eq!(
        RestartPolicy::OneForAll.decision(ChildRelation::AfterFailed),
        RestartDecision::Restart
    );
}

#[test]
fn rest_for_one_restarts_failed_child_and_younger_siblings_only() {
    assert_eq!(
        RestartPolicy::RestForOne.decision(ChildRelation::BeforeFailed),
        RestartDecision::KeepRunning
    );
    assert_eq!(
        RestartPolicy::RestForOne.decision(ChildRelation::Failed),
        RestartDecision::Restart
    );
    assert_eq!(
        RestartPolicy::RestForOne.decision(ChildRelation::AfterFailed),
        RestartDecision::Restart
    );
}

#[test]
fn child_relation_classifies_ordinals_relative_to_the_failure() {
    assert_eq!(
        ChildRelation::from_ordinals(1, 3),
        ChildRelation::BeforeFailed
    );
    assert_eq!(ChildRelation::from_ordinals(3, 3), ChildRelation::Failed);
    assert_eq!(
        ChildRelation::from_ordinals(5, 3),
        ChildRelation::AfterFailed
    );
}

#[test]
fn restart_budget_tracks_consumption_until_exhaustion() {
    let budget = RestartBudget::new(2);
    let tracker = budget.tracker();

    assert_eq!(tracker.budget(), budget);
    assert_eq!(tracker.restarts_used(), 0);
    assert_eq!(tracker.restarts_remaining(), 2);
    assert!(!tracker.is_exhausted());

    let tracker = tracker
        .record_restart()
        .expect("first restart should be allowed");
    assert_eq!(tracker.restarts_used(), 1);
    assert_eq!(tracker.restarts_remaining(), 1);
    assert!(!tracker.is_exhausted());

    let tracker = tracker
        .record_restart()
        .expect("second restart should still be allowed");
    assert_eq!(tracker.restarts_used(), 2);
    assert_eq!(tracker.restarts_remaining(), 0);
    assert!(tracker.is_exhausted());
}

#[test]
fn restart_budget_rejects_restarts_after_exhaustion() {
    let tracker = RestartBudget::new(1)
        .tracker()
        .record_restart()
        .expect("first restart should be allowed");

    let error = tracker
        .record_restart()
        .expect_err("second restart should exceed the budget");

    assert_eq!(error.attempted_restart(), 2);
    assert_eq!(error.max_restarts(), 1);
}

#[test]
fn restart_budget_state_can_be_reset_for_a_new_window() {
    let tracker = RestartBudget::new(3)
        .tracker()
        .record_restart()
        .expect("first restart should be allowed")
        .record_restart()
        .expect("second restart should be allowed");

    let reset = tracker.reset();

    assert_eq!(reset.restarts_used(), 0);
    assert_eq!(reset.restarts_remaining(), 3);
    assert!(!reset.is_exhausted());
}
