//! Registration and address-book helpers on [`Runtime`].
//!
//! This module owns every method that lands isolates in the runtime's
//! registry, supervises them, and routes deliveries between
//! [`RegisteredEntry`] rows. Three families live here:
//!
//! - public registration APIs (`register`, `register_with_capacity`,
//!   `register_with_capacity_and_bootstrap`,
//!   `register_with_capacity_using`, `register_service`,
//!   `register_service_send_only`, `register_split_service`,
//!   `supervise`, `try_supervise`);
//! - registered-address bookkeeping (`entry_index`, `entry_by_isolate`,
//!   `child_record_index_by_child`, `supervisor_index`,
//!   `try_registered_address`);
//! - spawn / bootstrap / mailbox plumbing (`spawn_isolate`,
//!   `record_child`, `enqueue_bootstrap_message`,
//!   `enqueue_entry_message`, `recv_entry_message`, plus the
//!   `register_entry` / `register_sendable_*` variants).
//!
//! Same-shape `Sendable` variants exist for the `ThreadedRuntime` lane;
//! they sit alongside the `!Send` variants so the two stay in sync.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::marker::PhantomData;

use tina::{
    Address, AddressGeneration, Isolate, IsolateId, Mailbox, Outbound as TinaOutbound, Shard,
    TrySendError,
};
use tina_supervisor::SupervisorConfig;

/// Where a non-droppable continuation landed. Lets the caller record the
/// honest trace (mailbox accept vs. overflow park) without changing terminal
/// outcome — both mean "delivered."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContinuationDelivery {
    Mailbox,
    Overflow,
}

use crate::call::IntoErasedCall;
use crate::dispatch::ErasedRestartRecipe;
use crate::errors::{RegisterBootstrapError, SuperviseError};
use crate::fact::IntoRuntimeFact;
use crate::mailbox::MailboxFactory;
use crate::trace::{CauseId, RuntimeEventKind};
use crate::{
    AnyMailboxAdapter, ChildRecord, DeliveredMessage, ErasedMailbox, HandlerAdapter,
    IntoErasedSpawn, IntoErasedSpawnObserved, IntoSendErasedSpawnObserved, MailboxAdapter,
    MessageCallContext, RegisteredAddress, RegisteredEntry, Runtime, SendOnlyServiceHandle,
    SendableHandlerAdapter, ServiceHandle, SpawnOutcome, SplitServiceHandle, SupervisorRecord,
};

impl<S, F> Runtime<S, F>
where
    S: Shard,
    F: MailboxFactory,
{
    fn create_mailbox<T: 'static>(&self, capacity: usize) -> Box<dyn Mailbox<T>> {
        self.mailbox_factory.create::<T>(capacity)
    }

    /// Registers one isolate and returns its typed address.
    ///
    /// Isolate identifiers are assigned in registration order, starting at `1`.
    #[allow(private_bounds)]
    pub fn register<I, M, Outbound>(
        &mut self,
        isolate: I,
        mailbox: M,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
        M: Mailbox<I::Message> + 'static,
    {
        let address = self.register_entry::<I, Outbound>(
            isolate,
            None,
            Box::new(MailboxAdapter::<M, I::Message> {
                mailbox,
                marker: PhantomData,
            }),
        );

        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    /// Registers one isolate and lets the runtime allocate the mailbox.
    #[allow(private_bounds)]
    pub fn register_with_capacity<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let address = self.register_entry::<I, Outbound>(
            isolate,
            None,
            Box::new(AnyMailboxAdapter {
                mailbox: self.create_mailbox::<Box<dyn Any>>(mailbox_capacity),
            }),
        );

        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    /// Registers one isolate as a callable service and returns capability-typed
    /// handles for the `.send` and `.call` lanes.
    ///
    /// The returned [`ServiceHandle`] exposes a [`SendAddress`](tina::SendAddress)
    /// for ordinary send/continuation traffic and a
    /// [`CallAddress`](tina::CallAddress) for callable traffic. Mixing the two
    /// becomes a compile error at the call boundary instead of a runtime
    /// `CallRejectedReason::UnsupportedMessage`.
    ///
    /// `I` must implement [`tina::CallableIsolate`]. The
    /// `#[tina::isolate]` / `#[tina_runtime::isolate]` macros emit that impl
    /// automatically when the impl block defines `fn handle_call(...)`. An
    /// isolate without `handle_call` is not callable; registering it through
    /// `register_service` is a compile error rather than a service whose every
    /// caller silently sees `UnsupportedMessage`. Use
    /// [`register_service_send_only`](Self::register_service_send_only) for the
    /// send-only shape.
    ///
    /// Negative fixture: an isolate without `handle_call` is not a callable
    /// service.
    ///
    /// ```compile_fail
    /// use std::convert::Infallible;
    /// use tina::prelude::*;
    /// use tina_runtime::{DefaultMailboxFactory, Runtime};
    ///
    /// #[derive(Debug)]
    /// enum Msg { Tick }
    ///
    /// struct NoCallHandler;
    ///
    /// #[tina_runtime::isolate(message = Msg)]
    /// impl NoCallHandler {
    ///     fn handle(
    ///         &mut self,
    ///         _msg: Msg,
    ///         _ctx: &mut Context<'_, SingleShard, Self::Reply>,
    ///     ) -> Effect<Self> {
    ///         noop()
    ///     }
    /// }
    ///
    /// let mut runtime = Runtime::new(SingleShard, DefaultMailboxFactory);
    /// // `NoCallHandler` is not a callable service.
    /// let _ = runtime.register_service::<NoCallHandler, Infallible>(NoCallHandler, 4);
    /// ```
    ///
    /// Internally this is exactly [`register_with_capacity`](Self::register_with_capacity);
    /// the raw [`Address`] stays available through
    /// [`CallAddress::address`](tina::CallAddress::address) for low-level
    /// interop.
    #[allow(private_bounds)]
    pub fn register_service<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> ServiceHandle<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + tina::CallableIsolate + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let address = self.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity);
        ServiceHandle::from_address(address)
    }

    /// Registers one isolate as a send-only service.
    ///
    /// Returned [`SendOnlyServiceHandle`] exposes only the `.send` lane. The
    /// isolate must declare `type Reply = ()` (or wrap its reply as
    /// [`std::convert::Infallible`]) so callers cannot construct a
    /// [`tina::CallAddress`] in the first place.
    #[allow(private_bounds)]
    pub fn register_service_send_only<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> SendOnlyServiceHandle<I::Message>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>, Reply = ()> + 'static,
        I::Message: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let address = self.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity);
        SendOnlyServiceHandle {
            send: address.send_only(),
        }
    }

    /// Registers one split event/request service.
    ///
    /// The isolate's message type must be [`tina::ServiceMessage<Event,
    /// Request>`], which the `#[tina_runtime::isolate(event = ..., request =
    /// ..., reply = ...)]` macro emits. The returned handle exposes separate
    /// event and request capabilities so ordinary code cannot send requests as
    /// events or call events as requests.
    #[allow(private_bounds)]
    pub fn register_split_service<I, Event, Request, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> SplitServiceHandle<Event, Request, I::Reply>
    where
        I: Isolate<
                Shard = S,
                Message = tina::ServiceMessage<Event, Request>,
                Send = TinaOutbound<Outbound>,
            > + tina::CallableIsolate
            + 'static,
        Event: 'static,
        Request: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let address = self.register_with_capacity::<I, Outbound>(isolate, mailbox_capacity);
        SplitServiceHandle::from_address(address)
    }

    /// Registers one isolate and prefills its mailbox with `bootstrap` so the
    /// first delivered message is always `bootstrap`.
    ///
    /// The mailbox is allocated, `bootstrap` is admitted via the mailbox's
    /// own `try_send`, and only then is the isolate entry inserted into the
    /// registry. If the prefill fails, no entry is created and no address is
    /// returned. There is no cleanup-after-registration path.
    ///
    /// Honesty:
    /// - the returned address may have a full mailbox until `bootstrap` is
    ///   delivered. Sending immediately after this call can see `Full`.
    /// - bootstrap delivery is an ordinary trace-visible mailbox event.
    /// - there is no special lifecycle callback.
    #[allow(private_bounds, clippy::type_complexity)]
    pub fn register_with_capacity_and_bootstrap<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: I::Message,
    ) -> Result<Address<I::Message, I::Reply>, RegisterBootstrapError<I::Message>>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let mailbox = self.create_mailbox::<Box<dyn Any>>(mailbox_capacity);
        let adapter = AnyMailboxAdapter { mailbox };
        let boxed: Box<dyn Any> = Box::new(bootstrap);
        if let Err(err) = adapter.try_send_boxed(boxed) {
            let recover = |b: Box<dyn Any>| {
                *b.downcast::<I::Message>()
                    .expect("bootstrap message type recovered from boxed Any")
            };
            return Err(match err {
                TrySendError::Full(b) => RegisterBootstrapError::Full(recover(b)),
                TrySendError::Closed(b) => RegisterBootstrapError::Closed(recover(b)),
            });
        }
        let address = self.register_entry::<I, Outbound>(isolate, None, Box::new(adapter));
        Ok(Address::new_with_generation(
            address.shard,
            address.isolate,
            address.generation,
        ))
    }

    /// Registers one isolate; constructor receives its own typed address.
    ///
    /// Replaces the `Begin { self_addr }` / `Bind { self_addr }` bootstrap
    /// variant: the closure gets the final `Address` and returns the isolate.
    /// Generation matches plain
    /// [`register_with_capacity`](Self::register_with_capacity) (initial 0).
    ///
    /// Honesty:
    /// - No message delivers before `construct` returns. The entry
    ///   lands in the registry only after the closure produces the
    ///   isolate value.
    /// - The closure has no runtime handle, so an address can only
    ///   escape through user-captured shared state.
    /// - Constructor panic *or* mailbox-create panic leaves the
    ///   allocated id with no entry. The id is never reused. A
    ///   `try_send` to a leaked address panics like any send to an
    ///   unknown id.
    /// - `construct` runs synchronously. Heavy work blocks the
    ///   caller; on `ThreadedRuntime` it blocks the worker thread
    ///   and starves every other isolate on that shard — build the
    ///   value before calling.
    #[allow(private_bounds)]
    pub fn register_with_capacity_using<I, Outbound, Ctor>(
        &mut self,
        mailbox_capacity: usize,
        construct: Ctor,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
        Ctor: FnOnce(Address<I::Message, I::Reply>) -> I,
    {
        let isolate_id = IsolateId::new(self.next_isolate_id);
        self.next_isolate_id += 1;
        let generation = AddressGeneration::new(0);
        let self_addr = Address::<I::Message, I::Reply>::new_with_generation(
            self.shard.id(),
            isolate_id,
            generation,
        );

        let mailbox = self.create_mailbox::<Box<dyn Any>>(mailbox_capacity);

        // Constructor panic leaves an allocated id with no entry. id
        // allocator is monotonic so leaking one is harmless.
        let isolate = construct(self_addr);

        let entry_index = self.entries.len();
        self.entries.push(RegisteredEntry {
            id: isolate_id,
            generation,
            parent: None,
            stopped: Cell::new(false),
            stopped_event: Cell::new(None),
            mailbox: Box::new(AnyMailboxAdapter { mailbox }),
            call_contexts: RefCell::new(VecDeque::new()),
            continuation_overflow: RefCell::new(VecDeque::new()),
            handler: RefCell::new(Box::new(HandlerAdapter::<I, Outbound> {
                isolate,
                marker: PhantomData,
            })),
        });
        self.entry_indexes.insert(isolate_id, entry_index);

        self_addr
    }

    /// Configures a registered isolate as supervisor for its direct children.
    ///
    /// This is a setup-time runtime API. Unknown, stale, or cross-shard parent
    /// addresses are programmer errors and panic. Reconfiguring the same parent
    /// replaces the config and resets the runtime-lifetime budget tracker.
    pub fn supervise<M: 'static, R>(&mut self, parent: Address<M, R>, config: SupervisorConfig) {
        // Keep the panicking surface for callers who want a setup-time
        // assertion, but route the actual work through the fallible
        // `try_supervise` so the panic message stays in one place.
        if self.try_supervise(parent, config).is_err() {
            panic!(
                "supervise expected an address registered with this runtime, got an unknown or stale address",
            );
        }
    }

    /// Configures one registered isolate as supervisor without panicking on
    /// unknown parents.
    ///
    /// The panicking [`supervise`](Self::supervise) variant remains
    /// available for setup code that wants the unknown-parent case to
    /// be a hard programmer error. `try_supervise` is the fallible
    /// variant that [`crate::ThreadedRuntime`] uses internally so an
    /// unknown-parent registration does not crash the worker thread.
    pub fn try_supervise<M: 'static, R>(
        &mut self,
        parent: Address<M, R>,
        config: SupervisorConfig,
    ) -> Result<(), SuperviseError> {
        let Some(parent) = self.try_registered_address(parent) else {
            return Err(SuperviseError::UnknownParent);
        };
        let budget_state = config.budget().tracker();

        if let Some(record) = self
            .supervisors
            .iter_mut()
            .find(|record| record.parent == parent)
        {
            record.config = config;
            record.budget_state = budget_state;
            return Ok(());
        }

        self.supervisors.push(SupervisorRecord {
            parent,
            config,
            budget_state,
        });
        Ok(())
    }

    pub(crate) fn enqueue_bootstrap_message(
        &mut self,
        child: RegisteredAddress,
        message: Box<dyn Any>,
        cause: CauseId,
    ) {
        let entry_index = self
            .entries
            .iter()
            .position(|entry| entry.id == child.isolate && entry.generation == child.generation)
            .unwrap_or_else(|| panic!("bootstrap referenced unknown child {:?}", child.isolate));
        self.enqueue_entry_message(entry_index, message, None)
            .unwrap_or_else(|_| {
                panic!(
                    "runtime failed to enqueue bootstrap message for child {:?}",
                    child.isolate
                )
            });
        self.push_event(
            child.isolate,
            Some(cause),
            RuntimeEventKind::MailboxAccepted,
        );
    }

    pub(crate) fn enqueue_entry_message(
        &self,
        entry_index: usize,
        message: Box<dyn Any>,
        call_context: Option<MessageCallContext>,
    ) -> Result<(), TrySendError<Box<dyn Any>>> {
        match self.entries[entry_index].mailbox.try_send_boxed(message) {
            Ok(()) => {
                self.entries[entry_index]
                    .call_contexts
                    .borrow_mut()
                    .push_back(call_context);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn recv_entry_message(&self, entry_index: usize) -> Option<DeliveredMessage> {
        // Overflowed continuations drain first. This is an explicit priority
        // lane, not FIFO with the ordinary mailbox: the mailbox was full when
        // the continuation arrived, and the continuation keeps a held resource
        // alive, so liveness wins over ordinary queued ingress.
        if let Some(delivered) = self.entries[entry_index]
            .continuation_overflow
            .borrow_mut()
            .pop_front()
        {
            return Some(delivered);
        }
        let message = self.entries[entry_index].mailbox.recv_boxed()?;
        let call_context = self.entries[entry_index]
            .call_contexts
            .borrow_mut()
            .pop_front()
            .unwrap_or(None);
        Some(DeliveredMessage {
            message,
            call_context,
        })
    }

    /// True when the entry has any deliverable message — overflowed
    /// continuation or mailbox. Used by the skip-empty scan so an
    /// overflow-only entry is never skipped.
    pub(crate) fn entry_has_pending_message(&self, entry_index: usize) -> bool {
        !self.entries[entry_index]
            .continuation_overflow
            .borrow()
            .is_empty()
            || !self.entries[entry_index].mailbox.is_empty()
    }

    /// Delivers a runtime-call continuation that must not be dropped. Tries
    /// the bounded mailbox first; on `Full` it parks the message in the
    /// entry's priority overflow rather than dropping it, so a held resource's
    /// self-continuation always reaches the isolate. Returns `Err` only when
    /// the requester is gone (`Closed`), which is a real terminal.
    pub(crate) fn enqueue_call_continuation(
        &self,
        entry_index: usize,
        message: Box<dyn Any>,
        call_context: Option<MessageCallContext>,
    ) -> Result<ContinuationDelivery, TrySendError<Box<dyn Any>>> {
        match self.entries[entry_index].mailbox.try_send_boxed(message) {
            Ok(()) => {
                self.entries[entry_index]
                    .call_contexts
                    .borrow_mut()
                    .push_back(call_context);
                Ok(ContinuationDelivery::Mailbox)
            }
            Err(TrySendError::Full(message)) => {
                self.entries[entry_index]
                    .continuation_overflow
                    .borrow_mut()
                    .push_back(DeliveredMessage {
                        message,
                        call_context,
                    });
                Ok(ContinuationDelivery::Overflow)
            }
            Err(closed @ TrySendError::Closed(_)) => Err(closed),
        }
    }

    pub(crate) fn entry_index(&self, address: RegisteredAddress) -> Option<usize> {
        let index = *self.entry_indexes.get(&address.isolate)?;
        let entry = self.entries.get(index)?;
        (entry.generation == address.generation).then_some(index)
    }

    pub(crate) fn entry_by_isolate(&self, isolate: IsolateId) -> Option<&RegisteredEntry<S, F>> {
        self.entries.iter().find(|entry| entry.id == isolate)
    }

    pub(crate) fn rebuild_entry_indexes(&mut self) {
        self.entry_indexes.clear();
        self.entry_indexes.extend(
            self.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.id, index)),
        );
    }

    pub(crate) fn child_record_index_by_child(&self, child: RegisteredAddress) -> Option<usize> {
        self.child_records
            .iter()
            .position(|record| record.child == child && record.remote_owner.is_none())
    }

    pub(crate) fn supervisor_index(&self, parent: IsolateId) -> Option<usize> {
        self.supervisors
            .iter()
            .position(|record| record.parent.isolate == parent)
    }

    pub(crate) fn try_registered_address<M: 'static, R>(
        &self,
        address: Address<M, R>,
    ) -> Option<RegisteredAddress> {
        if address.shard() != self.shard.id() {
            return None;
        }

        let entry = self
            .entries
            .iter()
            .find(|entry| entry.id == address.isolate())?;

        if entry.generation != address.generation() {
            return None;
        }

        Some(RegisteredAddress {
            shard: address.shard(),
            isolate: address.isolate(),
            generation: address.generation(),
        })
    }

    pub(crate) fn register_entry<I, Outbound>(
        &mut self,
        isolate: I,
        parent: Option<IsolateId>,
        mailbox: Box<dyn ErasedMailbox>,
    ) -> RegisteredAddress
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let isolate_id = IsolateId::new(self.next_isolate_id);
        self.next_isolate_id += 1;
        let generation = AddressGeneration::new(0);

        let entry_index = self.entries.len();
        self.entries.push(RegisteredEntry {
            id: isolate_id,
            generation,
            parent,
            stopped: Cell::new(false),
            stopped_event: Cell::new(None),
            mailbox,
            call_contexts: RefCell::new(VecDeque::new()),
            continuation_overflow: RefCell::new(VecDeque::new()),
            handler: RefCell::new(Box::new(HandlerAdapter::<I, Outbound> {
                isolate,
                marker: PhantomData,
            })),
        });
        self.entry_indexes.insert(isolate_id, entry_index);

        RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation,
        }
    }

    /// Registers a child for `spawn_observed(...).on_shard(owner_or_other)`.
    ///
    /// When `owner` is on *this* shard (the degenerate `.on_shard(my_shard)`
    /// case) the child is registered as a normal owned child of the owner — a
    /// `ChildRecord` is recorded and `Spawned` is emitted under the parent — so
    /// it matches local `spawn_observed` for ownership and lifecycle:
    /// `StopChildren` reaches it, and lineage and reports see it. (One trace
    /// nuance: the continuation send is caused by the handler-finished event
    /// rather than the `Spawned` event, so the causality edge differs slightly
    /// from local `spawn_observed`.) When `owner` is on another shard the child
    /// has no local parent (`parent = None`) and `Spawned` is recorded under the
    /// child on its own shard. Returns the new address.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_remote_child<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap_message: Option<I::Message>,
        owner: RegisteredAddress,
        child_ordinal: usize,
        remote_request_id: Option<crate::CallId>,
        remote_owner: Option<RegisteredAddress>,
        restart_recipe: Option<std::rc::Rc<dyn ErasedRestartRecipe<S, F>>>,
        cause: CauseId,
    ) -> RegisteredAddress
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        let local_parent = (owner.shard == self.shard.id()).then_some(owner.isolate);
        let child = self.register_entry::<I, Outbound>(
            isolate,
            local_parent,
            Box::new(AnyMailboxAdapter {
                mailbox: self.create_mailbox::<Box<dyn Any>>(mailbox_capacity),
            }),
        );
        let child_isolate = child.isolate;
        // A same-shard owner records a ChildRecord and attributes the `Spawned`
        // fact to the parent, exactly like local `spawn_observed`. A cross-shard
        // child records `Spawned` under itself (its owner is not local).
        let spawn_isolate = match local_parent {
            Some(parent) => {
                self.child_records.push(ChildRecord {
                    parent,
                    child,
                    child_ordinal,
                    mailbox_capacity,
                    restart_recipe,
                    remote_request_id: None,
                    remote_owner: None,
                    remote_restartable: false,
                });
                parent
            }
            None => {
                self.child_records.push(ChildRecord {
                    parent: owner.isolate,
                    child,
                    child_ordinal,
                    mailbox_capacity,
                    restart_recipe,
                    remote_request_id,
                    remote_owner,
                    remote_restartable: false,
                });
                child_isolate
            }
        };
        let spawned = self.push_event(
            spawn_isolate,
            Some(cause),
            RuntimeEventKind::Spawned { child_isolate },
        );
        if let Some(message) = bootstrap_message {
            self.enqueue_bootstrap_message(child, Box::new(message), spawned.into());
        }
        child
    }

    pub(crate) fn register_sendable_with_capacity<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
    ) -> Address<I::Message, I::Reply>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        let address = self.register_sendable_entry::<I, Outbound>(
            isolate,
            None,
            Box::new(AnyMailboxAdapter {
                mailbox: self.create_mailbox::<Box<dyn Any>>(mailbox_capacity),
            }),
        );

        Address::new_with_generation(address.shard, address.isolate, address.generation)
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn register_sendable_with_capacity_and_bootstrap<I, Outbound>(
        &mut self,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap: I::Message,
    ) -> Result<Address<I::Message, I::Reply>, RegisterBootstrapError<I::Message>>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        let mailbox = self.create_mailbox::<Box<dyn Any>>(mailbox_capacity);
        let adapter = AnyMailboxAdapter { mailbox };
        let boxed: Box<dyn Any> = Box::new(bootstrap);
        if let Err(err) = adapter.try_send_boxed(boxed) {
            let recover = |b: Box<dyn Any>| {
                *b.downcast::<I::Message>()
                    .expect("bootstrap message type recovered from boxed Any")
            };
            return Err(match err {
                TrySendError::Full(b) => RegisterBootstrapError::Full(recover(b)),
                TrySendError::Closed(b) => RegisterBootstrapError::Closed(recover(b)),
            });
        }
        let address = self.register_sendable_entry::<I, Outbound>(isolate, None, Box::new(adapter));
        Ok(Address::new_with_generation(
            address.shard,
            address.isolate,
            address.generation,
        ))
    }

    pub(crate) fn register_sendable_entry<I, Outbound>(
        &mut self,
        isolate: I,
        parent: Option<IsolateId>,
        mailbox: Box<dyn ErasedMailbox>,
    ) -> RegisteredAddress
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: Send + 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: Send + 'static,
    {
        let isolate_id = IsolateId::new(self.next_isolate_id);
        self.next_isolate_id += 1;
        let generation = AddressGeneration::new(0);

        let entry_index = self.entries.len();
        self.entries.push(RegisteredEntry {
            id: isolate_id,
            generation,
            parent,
            stopped: Cell::new(false),
            stopped_event: Cell::new(None),
            mailbox,
            call_contexts: RefCell::new(VecDeque::new()),
            continuation_overflow: RefCell::new(VecDeque::new()),
            handler: RefCell::new(Box::new(SendableHandlerAdapter::<I, Outbound> {
                isolate,
                marker: PhantomData,
            })),
        });
        self.entry_indexes.insert(isolate_id, entry_index);

        RegisteredAddress {
            shard: self.shard.id(),
            isolate: isolate_id,
            generation,
        }
    }

    pub(crate) fn spawn_isolate<I, Outbound>(
        &mut self,
        parent: IsolateId,
        isolate: I,
        mailbox_capacity: usize,
        bootstrap_message: Option<I::Message>,
    ) -> SpawnOutcome<S, F>
    where
        I: Isolate<Shard = S, Send = TinaOutbound<Outbound>> + 'static,
        I::Message: 'static,
        I::Reply: 'static,
        I::Spawn: IntoErasedSpawn<S, F> + 'static,
        I::SpawnObserved: IntoErasedSpawnObserved<S, F, I::Message> + 'static,
        I::SpawnObservedRemote: IntoSendErasedSpawnObserved<S, F, I::Message> + 'static,
        I::Call: IntoErasedCall<I::Message> + 'static,
        I::Fact: IntoRuntimeFact + 'static,
        Outbound: 'static,
    {
        if mailbox_capacity == 0 {
            panic!("spawn requested mailbox capacity 0, which is out of scope for this slice");
        }

        let child = self.register_entry::<I, Outbound>(
            isolate,
            Some(parent),
            Box::new(AnyMailboxAdapter {
                mailbox: self.create_mailbox::<Box<dyn Any>>(mailbox_capacity),
            }),
        );

        SpawnOutcome {
            child,
            mailbox_capacity,
            restart_recipe: None,
            bootstrap_message: bootstrap_message.map(|message| Box::new(message) as Box<dyn Any>),
        }
    }

    pub(crate) fn record_child(&mut self, parent: IsolateId, outcome: SpawnOutcome<S, F>) {
        let child_ordinal = self
            .child_records
            .iter()
            .filter(|record| record.parent == parent && record.remote_owner.is_none())
            .count();

        self.child_records.push(ChildRecord {
            parent,
            child: outcome.child,
            child_ordinal,
            mailbox_capacity: outcome.mailbox_capacity,
            restart_recipe: outcome.restart_recipe,
            remote_request_id: None,
            remote_owner: None,
            remote_restartable: false,
        });
    }

    pub(crate) fn record_remote_child_on_owner(
        &mut self,
        parent: IsolateId,
        child: RegisteredAddress,
        child_ordinal: usize,
        mailbox_capacity: usize,
        remote_restartable: bool,
    ) {
        if self.child_records.iter().any(|record| {
            record.parent == parent
                && record.remote_owner.is_none()
                && record.child_ordinal == child_ordinal
        }) {
            return;
        }
        self.child_records.push(ChildRecord {
            parent,
            child,
            child_ordinal,
            mailbox_capacity,
            restart_recipe: None,
            remote_request_id: None,
            remote_owner: None,
            remote_restartable,
        });
    }
}
