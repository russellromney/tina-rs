use std::collections::VecDeque;

use tina::AddressGeneration;
use tina_runtime::{RestartSkippedReason, RuntimeEventKind, SendRejectedReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SimAddress {
    isolate: u64,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct SimChild {
    address: SimAddress,
    live: bool,
    restartable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Report {
    Started(SimAddress),
    Stopped(SimAddress),
    Restarted {
        old: SimAddress,
        new: SimAddress,
    },
    RestartSkipped {
        old: SimAddress,
        reason: RestartSkippedReason,
    },
    ControlRejected(SendRejectedReason),
    ControlPressure,
}

#[derive(Debug)]
struct ProtocolModel {
    owner_live: bool,
    pending_spawn: bool,
    pending_restart: Option<SimAddress>,
    remote_child: Option<SimChild>,
    owner_child: Option<SimAddress>,
    cancel_tombstones: VecDeque<u64>,
    tombstone_capacity: usize,
    next_isolate: u64,
    reports: Vec<Report>,
}

impl ProtocolModel {
    fn new(tombstone_capacity: usize) -> Self {
        Self {
            owner_live: true,
            pending_spawn: false,
            pending_restart: None,
            remote_child: None,
            owner_child: None,
            cancel_tombstones: VecDeque::new(),
            tombstone_capacity,
            next_isolate: 1,
            reports: Vec::new(),
        }
    }

    fn spawn_requested(&mut self) {
        assert!(self.owner_live);
        self.pending_spawn = true;
    }

    fn spawn_request_arrives(&mut self, request_id: u64, restartable: bool) {
        if self.cancel_tombstones.iter().any(|id| *id == request_id) {
            return;
        }
        let address = SimAddress {
            isolate: self.next_isolate,
            generation: 0,
        };
        self.next_isolate += 1;
        self.remote_child = Some(SimChild {
            address,
            live: true,
            restartable,
        });
    }

    fn owner_stop(&mut self, request_id: u64, route_cancel: Result<(), SendRejectedReason>) {
        self.owner_live = false;
        if self.pending_spawn {
            match route_cancel {
                Ok(()) => {
                    self.remember_tombstone(request_id);
                    self.pending_spawn = false;
                }
                Err(reason) => self.reports.push(Report::ControlRejected(reason)),
            }
        }
        if let Some(child) = self.owner_child {
            self.stop_remote_child(child);
        }
    }

    fn spawn_reply_arrives(&mut self) {
        let Some(child) = self.remote_child.map(|child| child.address) else {
            self.pending_spawn = false;
            return;
        };
        if !self.pending_spawn {
            return;
        }
        self.pending_spawn = false;
        self.owner_child = Some(child);
        self.reports.push(Report::Started(child));
        if !self.owner_live {
            self.stop_remote_child(child);
        }
    }

    fn stop_children(&mut self) {
        if let Some(child) = self.owner_child {
            self.stop_remote_child(child);
        }
    }

    fn restart_children(&mut self) {
        let Some(old) = self.owner_child else {
            return;
        };
        let Some(child) = self.remote_child else {
            return;
        };
        if !child.restartable {
            self.reports.push(Report::RestartSkipped {
                old,
                reason: RestartSkippedReason::RemoteNotRestartable,
            });
            return;
        }
        self.pending_restart = Some(old);
    }

    fn restart_request_arrives(&mut self) {
        let Some(old) = self.pending_restart else {
            return;
        };
        if let Some(child) = self.remote_child.as_mut() {
            assert_eq!(child.address, old);
            child.live = false;
        }
        let new = SimAddress {
            isolate: self.next_isolate,
            generation: 0,
        };
        self.next_isolate += 1;
        self.remote_child = Some(SimChild {
            address: new,
            live: true,
            restartable: true,
        });
    }

    fn restart_reply_arrives(&mut self) {
        let Some(old) = self.pending_restart.take() else {
            return;
        };
        let new = self
            .remote_child
            .expect("restart request created replacement")
            .address;
        self.owner_child = Some(new);
        self.reports.push(Report::Restarted { old, new });
    }

    fn remember_tombstone(&mut self, request_id: u64) {
        if self.tombstone_capacity == 0 {
            self.reports.push(Report::ControlPressure);
            return;
        }
        if self.cancel_tombstones.len() == self.tombstone_capacity {
            self.cancel_tombstones.pop_front();
            self.reports.push(Report::ControlPressure);
        }
        self.cancel_tombstones.push_back(request_id);
    }

    fn stop_remote_child(&mut self, child: SimAddress) {
        if let Some(remote) = self.remote_child.as_mut() {
            assert_eq!(remote.address, child);
            remote.live = false;
        }
        self.reports.push(Report::Stopped(child));
    }

    fn remote_child_live(&self, address: SimAddress) -> bool {
        self.remote_child
            .is_some_and(|child| child.address == address && child.live)
    }
}

#[test]
fn cross_shard_child_ownership_sim_trace_vocabulary_is_available() {
    let _ = RuntimeEventKind::RemoteChildControlPressure { capacity: 4 };
    let _ = RuntimeEventKind::RestartChildSkipped {
        child_ordinal: 0,
        old_isolate: tina::IsolateId::new(1),
        old_generation: AddressGeneration::new(0),
        reason: RestartSkippedReason::RemoteNotRestartable,
    };
}

#[test]
fn deterministic_spawn_reply_then_stop_children_closes_remote_child() {
    let mut model = ProtocolModel::new(2);
    model.spawn_requested();
    model.spawn_request_arrives(1, false);
    model.spawn_reply_arrives();
    let child = model.owner_child.expect("owner learned child");

    model.stop_children();

    assert!(!model.remote_child_live(child));
    assert_eq!(
        model.reports,
        vec![Report::Started(child), Report::Stopped(child)]
    );
}

#[test]
fn deterministic_owner_stop_cancel_before_spawn_request_tombstones_request() {
    let mut model = ProtocolModel::new(2);
    model.spawn_requested();

    model.owner_stop(1, Ok(()));
    model.spawn_request_arrives(1, false);
    model.spawn_reply_arrives();

    assert!(model.owner_child.is_none());
    assert!(model.remote_child.is_none());
    assert!(model.reports.is_empty());
}

#[test]
fn deterministic_cancel_full_retains_pending_spawn_and_late_reply_stops_child() {
    let mut model = ProtocolModel::new(1);
    model.spawn_requested();

    model.owner_stop(1, Err(SendRejectedReason::Full));
    model.spawn_request_arrives(1, false);
    model.spawn_reply_arrives();
    let child = model
        .owner_child
        .expect("late reply records the child first");

    assert!(!model.remote_child_live(child));
    assert_eq!(
        model.reports,
        vec![
            Report::ControlRejected(SendRejectedReason::Full),
            Report::Started(child),
            Report::Stopped(child),
        ]
    );
}

#[test]
fn deterministic_restart_replaces_remote_child_and_stales_old_address() {
    let mut model = ProtocolModel::new(2);
    model.spawn_requested();
    model.spawn_request_arrives(1, true);
    model.spawn_reply_arrives();
    let old = model.owner_child.expect("owner learned child");

    model.restart_children();
    model.restart_request_arrives();
    model.restart_reply_arrives();
    let new = model.owner_child.expect("owner learned replacement");

    assert_ne!(old, new);
    assert!(!model.remote_child_live(old));
    assert!(model.remote_child_live(new));
    assert!(model.reports.contains(&Report::Restarted { old, new }));
}

#[test]
fn deterministic_restart_skips_non_restartable_remote_child() {
    let mut model = ProtocolModel::new(2);
    model.spawn_requested();
    model.spawn_request_arrives(1, false);
    model.spawn_reply_arrives();
    let child = model.owner_child.expect("owner learned child");

    model.restart_children();

    assert!(model.remote_child_live(child));
    assert!(model.reports.contains(&Report::RestartSkipped {
        old: child,
        reason: RestartSkippedReason::RemoteNotRestartable,
    }));
}

#[test]
fn deterministic_tombstone_table_is_bounded_and_reports_pressure() {
    let mut model = ProtocolModel::new(1);

    model.remember_tombstone(1);
    model.remember_tombstone(2);

    assert_eq!(model.cancel_tombstones.len(), 1);
    assert_eq!(model.cancel_tombstones[0], 2);
    assert!(model.reports.contains(&Report::ControlPressure));
}
