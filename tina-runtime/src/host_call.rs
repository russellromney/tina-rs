//! Host-side helpers on [`Runtime`] for tests, drivers, and live
//! supervisors.
//!
//! This module collects the methods a host (test harness, threaded
//! runtime, local system) calls from outside a handler turn:
//!
//! - introspection: [`Runtime::has_in_flight_calls`], [`Runtime::trace`],
//!   [`Runtime::pressure_summary`], and the test-only counters;
//! - ingress: [`Runtime::try_send`] / [`Runtime::try_send_event`];
//! - typed observers: the `observe_*` family that returns one-shot
//!   waiters keyed on existing trace events;
//! - trace policy: [`Runtime::set_trace_retention`] /
//!   [`Runtime::set_trace_observer`];
//! - test snapshots: lineage / child-record / supervisor snapshots.
//!
//! Constructors and pure const accessors (`shard`, `trace_retention`,
//! `trace_dropped`, `cancelled_call_cause_evictions`) stay in
//! [`crate::lib`](crate) so they read as part of the struct's own
//! definition.

use std::sync::Arc;

use tina::{Address, Shard, TrySendError};

use crate::driver::DriverResourceReport;
use crate::mailbox::MailboxFactory;
use crate::observation::{
    BoundAddressWaiter, ChildRestartedWaiter, IsolateCompleteWaiter, IsolateResultWaiter,
    OperationDoneWaiter, ResultWaitError,
};
use crate::trace::{CallKind, RuntimeEvent};
use crate::{
    ChildLifecycleReport, ChildLifecycleReportError, RegisteredAddress, Runtime, TraceObserver,
    TraceRetention, pressure,
};
#[cfg(test)]
use crate::{ChildRecordSnapshot, SupervisorRecordSnapshot};

impl<S, F> Runtime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    /// Returns whether the runtime has any in-flight calls that have not
    /// yet been delivered. Tests use this to know when stepping further
    /// can produce more I/O completions.
    pub fn has_in_flight_calls(&self) -> bool {
        self.call_table.has_driver_calls()
            || self.driver.has_pending()
            || self.call_table.has_isolate_calls()
    }

    #[cfg(test)]
    pub(crate) fn io_pending_count(&self) -> usize {
        self.driver.io_pending_count()
    }

    pub(crate) fn resource_report(&self) -> DriverResourceReport {
        self.driver.resource_report()
    }

    /// Returns the accumulated runtime trace.
    pub fn trace(&self) -> &[RuntimeEvent] {
        &self.trace[self.trace_start..]
    }

    #[cfg(test)]
    pub(crate) fn trace_storage_len(&self) -> usize {
        self.trace.len()
    }

    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Walks the current trace and returns a counted summary of
    /// pressure-shaped events (mailbox-full, reply-path-full,
    /// send-full, lifecycle-closed). See [`crate::PressureSummary`].
    pub fn pressure_summary(&self) -> pressure::PressureSummary {
        pressure::PressureSummary::from_events(self.trace.iter())
    }

    /// Registers a typed waiter for the next `tcp_bind` completion.
    ///
    /// Returns a [`BoundAddressWaiter`] that the host can `wait` on to
    /// receive the bound `SocketAddr` (or a typed error). Each call returns
    /// a fresh waiter; multiple registrations are served in registration
    /// order as `tcp_bind` calls complete. The waiter is bounded one-slot:
    /// no hidden queue is created.
    ///
    /// The trace remains the source of audit truth: this method does not
    /// add a new event class, it only surfaces the bound address that
    /// [`crate::CallOutput::TcpBound`] already carries inside the runtime.
    pub fn observe_next_bound(&mut self) -> BoundAddressWaiter {
        self.observation.register_bound()
    }

    /// Registers a typed waiter for the next `tls_bind` completion.
    /// Mirrors [`Self::observe_next_bound`] for the TLS rail. The
    /// waiter resolves with the bound `SocketAddr` carried by
    /// [`crate::CallOutput::TlsBound`], or with the typed runtime error.
    pub fn observe_next_tls_bound(&mut self) -> BoundAddressWaiter {
        self.observation.register_tls_bound()
    }

    /// Registers a typed waiter for the targeted isolate's `IsolateStopped`.
    ///
    /// The waiter resolves the next time the isolate identified by `address`
    /// (matched by isolate id and generation) emits
    /// [`crate::RuntimeEventKind::IsolateStopped`]. Replaces `Arc<AtomicBool>` done
    /// flags in user code. Bounded one-slot.
    pub fn observe_isolate_complete<M, R>(
        &mut self,
        address: Address<M, R>,
    ) -> IsolateCompleteWaiter {
        self.observation
            .register_isolate_complete(address.isolate(), address.generation())
    }

    /// Registers a typed waiter for the next runtime call of `call_kind`
    /// issued by the isolate identified by `address` that completes (success
    /// or failure).
    ///
    /// Replaces `complete_trace()` polling for a specific
    /// `CallKind::TcpStreamClose` / `CallKind::Sleep` / etc. event in user
    /// code. Bounded one-slot; the runtime drops the slot once a matching
    /// completion lands.
    pub fn observe_operation_done<M, R>(
        &mut self,
        address: Address<M, R>,
        call_kind: CallKind,
    ) -> OperationDoneWaiter {
        self.observation
            .register_operation_done(address.isolate(), call_kind)
    }

    /// Registers a typed waiter for the next supervised restart of any
    /// direct child of the parent identified by `parent_address`.
    ///
    /// The resolved [`crate::ChildRestarted`] carries the new child
    /// incarnation's isolate id and generation. Bounded one-slot.
    pub fn observe_child_restarted<M, R>(
        &mut self,
        parent_address: Address<M, R>,
    ) -> ChildRestartedWaiter {
        self.observation
            .register_child_restarted(parent_address.isolate())
    }

    /// Returns the live runtime-owned lifecycle report for direct children of
    /// `parent_address`.
    pub fn child_lifecycle_report<M, R>(
        &self,
        parent_address: Address<M, R>,
    ) -> Result<ChildLifecycleReport, ChildLifecycleReportError> {
        if parent_address.shard() != self.shard.id() {
            return Err(ChildLifecycleReportError::ParentShardUnavailable(
                parent_address.shard(),
            ));
        }
        ChildLifecycleReport::from_runtime(
            self,
            RegisteredAddress {
                shard: parent_address.shard(),
                isolate: parent_address.isolate(),
                generation: parent_address.generation(),
            },
        )
    }

    /// Registers a typed result waiter for the isolate at `address`.
    ///
    /// Resolves when the isolate stops via [`tina::stop_with`] with a value
    /// of type `T`. Single-claim per `(IsolateId, AddressGeneration)`.
    /// Eager errors:
    ///
    /// - `AlreadyStopped` — isolate is no longer alive at this generation
    ///   (no replay cache);
    /// - `AlreadyClaimed` — another waiter holds the slot;
    /// - `ObservationFull` — observation cap reached.
    ///
    /// `wait` outcomes: `Timeout`, `RuntimeStopped`, `StoppedWithoutResult`
    /// (isolate used `stop()` not `stop_with(_)`), `TypeMismatch`.
    pub fn observe_result<T, M, R>(
        &mut self,
        address: Address<M, R>,
    ) -> Result<IsolateResultWaiter<T>, ResultWaitError>
    where
        T: Send + 'static,
    {
        let isolate = address.isolate();
        let generation = address.generation();
        let alive = self.entries.iter().any(|entry| {
            entry.id == isolate && entry.generation == generation && !entry.stopped.get()
        });
        if !alive {
            return Err(ResultWaitError::AlreadyStopped);
        }
        self.observation
            .register_isolate_result::<T>(isolate, generation)
    }

    /// Sets the trace retention policy for future events.
    ///
    /// Lowering retention trims the current trace immediately so callers can
    /// rely on the memory bound after this returns.
    pub fn set_trace_retention(&mut self, retention: TraceRetention) {
        self.trace_retention = retention;
        self.enforce_trace_retention();
    }

    /// Sets the live trace observer. `None` detaches. See
    /// [`crate::TraceObserver`] for hook rules. On `ThreadedRuntime` /
    /// `LocalSystem`, prefer the build-time wiring so no events fire
    /// before the hook is in place.
    pub fn set_trace_observer(&mut self, observer: Option<Arc<dyn TraceObserver>>) {
        self.trace_observer = observer;
    }

    /// Sets the per-step backend completion drain budget (>= 1). The live
    /// worker wires this from `ThreadedRuntimeConfig`; the explicit-step
    /// runtime and the simulator keep the deterministic default.
    pub fn set_driver_completion_drain_budget(&mut self, budget: usize) {
        self.driver_completion_drain_budget = budget.max(1);
    }

    /// Attempts to deliver `message` to a registered isolate.
    ///
    /// This is the runtime-side ingress surface for tests and later drivers.
    /// It preserves the mailbox's typed `Full` and `Closed` outcomes. Stopped,
    /// stale, and unknown isolate IDs all return `Closed` with the original
    /// message so host-side tests and drivers can handle the miss without a
    /// panic.
    pub fn try_send<M: 'static, R>(
        &self,
        address: Address<M, R>,
        message: M,
    ) -> Result<(), TrySendError<M>> {
        if address.shard() != self.shard.id() {
            panic!(
                "cross-shard runtime ingress is out of scope in this slice: target shard {} != runtime shard {}",
                address.shard().get(),
                self.shard.id().get(),
            );
        }

        // Resolve to entry index in O(1) via `entry_indexes`. The helper also
        // re-checks the address generation, so an unknown isolate and a stale
        // generation both surface as `Closed` here — same as before. The
        // previous code scanned `self.entries` twice (once via `find`, once
        // via `position`) to recover the same index.
        let Some(entry_index) = self.entry_index(RegisteredAddress {
            shard: address.shard(),
            isolate: address.isolate(),
            generation: address.generation(),
        }) else {
            return Err(TrySendError::Closed(message));
        };

        match self.enqueue_entry_message(entry_index, Box::new(message), None) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => Err(TrySendError::Full(
                *message.downcast::<M>().unwrap_or_else(|_| {
                    panic!(
                        "runtime ingress attempted to deliver a message to a mailbox with the wrong type"
                    )
                }),
            )),
            Err(TrySendError::Closed(message)) => Err(TrySendError::Closed(
                *message.downcast::<M>().unwrap_or_else(|_| {
                    panic!(
                        "runtime ingress attempted to deliver a message to a mailbox with the wrong type"
                    )
                }),
            )),
        }
    }

    /// Attempts to enqueue one public event through a split-service event
    /// capability.
    ///
    /// This is the host/runtime companion to [`tina::send_event`]. It keeps
    /// tests and setup code on the capability-typed path instead of unwrapping
    /// the raw `ServiceMessage<Event, Request>` address.
    pub fn try_send_event<Event: 'static, Request: 'static>(
        &self,
        address: tina::ServiceEventAddress<Event, Request>,
        event: Event,
    ) -> Result<(), TrySendError<tina::ServiceMessage<Event, Request>>> {
        self.try_send(
            address.address().address(),
            tina::ServiceMessage::Event(event),
        )
    }

    /// Returns the stored direct-parent lineage in registration order.
    #[cfg(test)]
    pub(crate) fn lineage_snapshot(&self) -> Vec<(tina::IsolateId, Option<tina::IsolateId>)> {
        self.entries
            .iter()
            .map(|entry| (entry.id, entry.parent))
            .collect()
    }

    /// Returns the stored child records in spawn order.
    #[cfg(test)]
    pub(crate) fn child_record_snapshot(&self) -> Vec<ChildRecordSnapshot> {
        self.child_records
            .iter()
            .map(|record| ChildRecordSnapshot {
                parent: record.parent,
                child_shard: record.child.shard,
                child_isolate: record.child.isolate,
                child_generation: record.child.generation,
                child_ordinal: record.child_ordinal,
                mailbox_capacity: record.mailbox_capacity,
                restartable: record.restart_recipe.is_some() || record.remote_restartable,
            })
            .collect()
    }

    /// Returns the stored supervisor records in configuration order.
    #[cfg(test)]
    pub(crate) fn supervisor_snapshot(&self) -> Vec<SupervisorRecordSnapshot> {
        self.supervisors
            .iter()
            .map(|record| SupervisorRecordSnapshot {
                parent: record.parent,
                config: record.config,
                budget_state: record.budget_state,
            })
            .collect()
    }
}
