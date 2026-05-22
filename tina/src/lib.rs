#![deny(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Core traits and data types for the `tina-rs` discipline.
//!
//! `tina` is intentionally a trait crate: it names the vocabulary that later
//! runtime crates will implement, but it does not ship a scheduler, mailbox,
//! or supervisor.
//!
//! # Effect Shape
//!
//! Phase Sputnik resolves the roadmap's first open question in favor of a
//! **closed** [`Effect`] enum rather than a per-isolate associated effect type.
//!
//! This keeps the dispatcher contract small and uniform: every isolate can only
//! ask for the same handful of verbs (`Reply`, `Send`, `Spawn`, `Stop`,
//! `RestartChildren`, `Call`, and ordered `Batch`). That simplicity matters
//! for the runtime crates we add in later phases, because they can switch on
//! one shared enum instead of handling an open-ended effect language for every
//! isolate.
//!
//! The tradeoff is that the effect *payloads* stay per-isolate via associated
//! types on [`Isolate`]. An isolate decides what a reply looks like, how it
//! packages an outbound send, and what data is needed to request a spawn, while
//! the dispatcher still sees one common envelope. The downside is that adding a
//! brand-new verb means changing this crate, not just defining a new associated
//! type. That is a deliberate Sputnik constraint.
//!
//! # Example
//!
//! The example below compiles and runs without a runtime because handlers only
//! build values; they do not perform I/O directly.
//!
//! ```
//! use tina::{
//!     send, reply, Address, Context, Effect, Isolate, IsolateId, Outbound, Shard, ShardId,
//! };
//!
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! enum Message {
//!     Add(u64),
//!     Read,
//! }
//!
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! enum AuditEvent {
//!     Total(u64),
//! }
//!
//! struct InlineShard;
//!
//! impl Shard for InlineShard {
//!     fn id(&self) -> ShardId {
//!         ShardId::new(0)
//!     }
//! }
//!
//! #[derive(Debug)]
//! struct Counter {
//!     total: u64,
//!     audit: Address<AuditEvent>,
//! }
//!
//! #[tina::isolate(
//!     message = Message,
//!     reply = u64,
//!     send = Outbound<AuditEvent>,
//!     shard = InlineShard
//! )]
//! impl Counter {
//!     fn handle(&mut self, msg: Message, _ctx: &mut Context<'_, InlineShard, Self::Reply>) -> Effect<Self> {
//!         match msg {
//!             Message::Add(delta) => {
//!                 self.total += delta;
//!                 send(self.audit, AuditEvent::Total(self.total))
//!             }
//!             Message::Read => reply(self.total),
//!         }
//!     }
//! }
//!
//! let audit = Address::new(ShardId::new(0), IsolateId::new(99));
//! let mut shard = InlineShard;
//! let mut ctx = Context::<_, <Counter as Isolate>::Reply>::new_typed(
//!     &mut shard,
//!     IsolateId::new(1),
//! );
//! let mut counter = Counter { total: 0, audit };
//!
//! match counter.handle(Message::Add(3), &mut ctx) {
//!     Effect::Send(outbound) => {
//!         let (destination, message) = outbound.into_parts();
//!         assert_eq!(destination, audit);
//!         assert_eq!(message, AuditEvent::Total(3));
//!     }
//!     _ => panic!("unexpected effect"),
//! }
//!
//! assert!(matches!(
//!     counter.handle(Message::Read, &mut ctx),
//!     Effect::Reply(3)
//! ));
//! ```

use std::any::Any;
use std::fmt;

/// Type-erased payload for [`Effect::StopWith`].
///
/// Holds the isolate's final value as `Box<dyn Any + Send>` so [`Effect`]
/// keeps no `Debug` bound on `T`. The host receives the value through
/// `runtime.observe_result::<T>(addr)`; type mismatch is a typed wait
/// outcome, not a panic.
pub struct StopResult(Box<dyn Any + Send>);

impl StopResult {
    /// Boxes a typed final value.
    pub fn new<T: Send + 'static>(value: T) -> Self {
        Self(Box::new(value))
    }

    /// Returns the inner boxed `Any` for runtime downcast.
    pub fn into_any(self) -> Box<dyn Any + Send> {
        self.0
    }

    /// Inner value's `TypeId`.
    pub fn type_id(&self) -> std::any::TypeId {
        (*self.0).type_id()
    }
}

impl fmt::Debug for StopResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StopResult")
            .field("type_id", &(*self.0).type_id())
            .finish()
    }
}

/// Declares a Tina isolate from an inherent `impl` block.
///
/// This is the preferred authoring path for ordinary Tina code. Only `message`
/// is required for single-shard isolates: omitted `shard = ...` defaults to
/// [`SingleShard`]. `reply`, `send`, `spawn`, and `call` default to the
/// no-reply/no-send/no-spawn/no-runtime-call shape.
///
/// ```compile_fail
/// struct DemoShard;
///
/// impl tina::Shard for DemoShard {
///     fn id(&self) -> tina::ShardId {
///         tina::ShardId::new(0)
///     }
/// }
///
/// struct Worker;
///
/// #[tina::isolate(message = (), shard = DemoShard)]
/// impl Worker {
///     async fn handle(
///         &mut self,
///         _msg: (),
///         _ctx: &mut tina::Context<'_, DemoShard, Self::Reply>,
///     ) -> tina::Effect<Self> {
///         tina::noop()
///     }
/// }
/// ```
pub use tina_macros::isolate;

// ---------------------------------------------------------------------------
// Module map (Phase 115 reorg)
// ---------------------------------------------------------------------------
//
// The `tina` trait crate is being split into module homes so future agents
// can find where new code belongs without scanning the old oversized file. Each
// split is a pure move: every public item stays reachable through the crate
// root via `pub use`.
//
//   - `mod address` — address/id/generation types, the `Outbound` wrapper,
//     and the service-shaped event/request addresses.
//   - `mod context` — handler context, request/call authority, deferred
//     replies, deadlines, call handles, and caller routing.
//   - `mod effect` — the closed effect language and effect constructors.
//   - `mod isolate` — isolate traits, mailboxes, shard traits, restart policy,
//     child definitions/refs, and observed-spawn builders.
mod address;
mod context;
mod effect;
mod isolate;
pub use address::*;
pub use context::*;
pub use effect::*;
pub use isolate::*;

/// Declares the associated-type slab for one [`Isolate`] impl.
///
/// This macro is intentionally small and boring. Prefer [`#[tina::isolate]`](isolate)
/// for ordinary code; use this when an explicit trait impl is clearer for
/// tests, generated code, or unusual boundaries.
///
/// ```
/// struct DemoShard;
///
/// impl tina::Shard for DemoShard {
///     fn id(&self) -> tina::ShardId {
///         tina::ShardId::new(0)
///     }
/// }
///
/// struct Worker;
///
/// impl tina::Isolate for Worker {
///     tina::isolate_types! {
///         message: (),
///         reply: (),
///         send: tina::Outbound<std::convert::Infallible>,
///         spawn: std::convert::Infallible,
///         call: std::convert::Infallible,
///         shard: DemoShard,
///     }
///
///     fn handle(
///         &mut self,
///         _msg: Self::Message,
///         _ctx: &mut tina::Context<'_, Self::Shard, Self::Reply>,
///     ) -> tina::Effect<Self> {
///         tina::stop()
///     }
/// }
/// ```
#[macro_export]
macro_rules! isolate_types {
    (
        message: $message:ty,
        reply: $reply:ty,
        send: $send:ty,
        spawn: $spawn:ty,
        spawn_observed: $spawn_observed:ty,
        call: $call:ty,
        fact: $fact:ty,
        shard: $shard:ty $(,)?
    ) => {
        type Message = $message;
        type Reply = $reply;
        type Send = $send;
        type Spawn = $spawn;
        type SpawnObserved = $spawn_observed;
        type Call = $call;
        type Fact = $fact;
        type Shard = $shard;
    };
    (
        message: $message:ty,
        reply: $reply:ty,
        send: $send:ty,
        spawn: $spawn:ty,
        spawn_observed: $spawn_observed:ty,
        call: $call:ty,
        shard: $shard:ty $(,)?
    ) => {
        $crate::isolate_types! {
            message: $message,
            reply: $reply,
            send: $send,
            spawn: $spawn,
            spawn_observed: $spawn_observed,
            call: $call,
            fact: ::core::convert::Infallible,
            shard: $shard,
        }
    };
    (
        message: $message:ty,
        reply: $reply:ty,
        send: $send:ty,
        spawn: $spawn:ty,
        call: $call:ty,
        fact: $fact:ty,
        shard: $shard:ty $(,)?
    ) => {
        $crate::isolate_types! {
            message: $message,
            reply: $reply,
            send: $send,
            spawn: $spawn,
            spawn_observed: ::core::convert::Infallible,
            call: $call,
            fact: $fact,
            shard: $shard,
        }
    };
    (
        message: $message:ty,
        reply: $reply:ty,
        send: $send:ty,
        spawn: $spawn:ty,
        call: $call:ty,
        shard: $shard:ty $(,)?
    ) => {
        $crate::isolate_types! {
            message: $message,
            reply: $reply,
            send: $send,
            spawn: $spawn,
            spawn_observed: ::core::convert::Infallible,
            call: $call,
            fact: ::core::convert::Infallible,
            shard: $shard,
        }
    };
}

/// The preferred first import for ordinary Tina application code.
/// Runtime-only conduits for deferred reply slot construction.
///
/// This module is **not** a stable public API. It exists so
/// `tina-runtime` and `tina-sim` can mint deferred reply handles and
/// extract them from [`DeferredReply`] when erasing effects. Reaching
/// into it from application code defeats the one-shot type-system
/// guarantees on [`reply_to`] (a duplicate reply becomes possible if
/// you rebuild a [`DeferredReply`] out of band) and may deliver
/// payloads of the wrong type to the runtime, panicking the
/// per-call translator.
///
/// If you find yourself wanting to call anything in here, write a
/// runtime-side helper instead.
#[doc(hidden)]
pub mod runtime_internal {
    use std::sync::Arc;

    use crate::{
        CallHandle, CallHandleInner, CallHandleShared, DeferredReply, DeferredReplyHandle,
        DeferredSlotShared, RequestContext,
    };

    /// Build a handle from a runtime-allocated shared slot.
    pub fn handle_from_shared(shared: Arc<DeferredSlotShared>) -> DeferredReplyHandle {
        DeferredReplyHandle { shared }
    }

    /// Borrow the shared slot for routing/sweep checks.
    pub fn handle_shared(handle: &DeferredReplyHandle) -> &Arc<DeferredSlotShared> {
        &handle.shared
    }

    /// Move the handle out of a typed slot. Used by erase paths.
    pub fn deferred_into_handle<R>(slot: DeferredReply<R>) -> DeferredReplyHandle {
        slot.handle
    }

    /// Wrap a handle into a typed slot. Used internally by
    /// [`crate::Context::take_reply_slot`]; runtime crates do not
    /// normally need this.
    pub fn deferred_from_handle<R>(handle: DeferredReplyHandle) -> DeferredReply<R> {
        DeferredReply {
            handle,
            _marker: std::marker::PhantomData,
        }
    }

    /// Borrow the inner handle of a typed slot without consuming it.
    /// Pair with [`handle_shared`] to clone the shared slot for an
    /// observer that survives `reply_to`.
    pub fn deferred_handle_ref<R>(slot: &DeferredReply<R>) -> &DeferredReplyHandle {
        &slot.handle
    }

    /// Move the deferred reply out of a [`RequestContext`].
    pub fn request_context_into_deferred<R>(req: RequestContext<R>) -> DeferredReply<R> {
        req.0
    }

    /// Wrap a deferred reply into a [`RequestContext`].
    pub fn request_context_from_deferred<R>(slot: DeferredReply<R>) -> RequestContext<R> {
        RequestContext(slot)
    }

    /// Build a typed [`CallHandle`] from a runtime-allocated shared cell.
    pub fn call_handle_from_shared<R>(shared: Arc<CallHandleShared>) -> CallHandle<R> {
        CallHandle {
            handle: CallHandleInner { shared },
            _marker: std::marker::PhantomData,
        }
    }

    /// Borrow the shared cell for runtime-side dispatch and lookup.
    pub fn call_handle_shared<R>(handle: &CallHandle<R>) -> &Arc<CallHandleShared> {
        &handle.handle.shared
    }

    /// Move the typed handle into its erased inner form. Used by
    /// `cancel_call`'s erase path to drop the reply-type marker.
    pub fn call_handle_into_inner<R>(handle: CallHandle<R>) -> CallHandleInner {
        handle.handle
    }

    /// Borrow the shared cell from an erased inner handle.
    pub fn call_handle_inner_shared(inner: &CallHandleInner) -> &Arc<CallHandleShared> {
        &inner.shared
    }

    /// Consume the erased inner handle, returning its shared cell.
    pub fn call_handle_inner_into_shared(inner: CallHandleInner) -> Arc<CallHandleShared> {
        inner.shared
    }

    /// Build a request-lane effect after caller authority has already been
    /// consumed.
    ///
    /// Runtime-side adapters use this after they have consumed a `RequestCall`
    /// into a `RequestContext`. It re-wraps an arbitrary [`crate::Effect`] into
    /// a [`crate::RequestEffect`], so it can manufacture the "caller answered"
    /// witness from a `noop()` — the exact bad state the split-service
    /// must-answer rail forbids.
    ///
    /// It is `unsafe` for that reason, not for memory safety: it is the one
    /// hole through the must-answer rail, so calling it is an explicit,
    /// `unsafe`-gated, reviewable opt-out that `#![forbid(unsafe_code)]` app
    /// crates reject outright. Ordinary code answers the caller through
    /// [`crate::RequestCall`] / [`crate::RequestContext`] instead.
    ///
    /// # Safety
    ///
    /// The caller must already have consumed the `RequestCall` authority for
    /// this request (e.g. via `RequestContext`), so that `effect` genuinely
    /// represents answering — or deliberately not answering — that one caller.
    /// Manufacturing a `RequestEffect` without consuming the authority breaks
    /// the "every call settles exactly once" invariant.
    #[allow(unsafe_code)]
    pub unsafe fn request_effect_from_consumed_effect<I>(
        effect: crate::Effect<I>,
    ) -> crate::RequestEffect<I>
    where
        I: crate::Isolate,
    {
        crate::RequestEffect::from_consumed_effect(effect)
    }
}

/// Common imports for ordinary Tina application code.
pub mod prelude {
    pub use crate::{
        Address, CallHandle, CallHandleState, CancelCause, CancelOutcome, ChildDefinition,
        ChildRef, Context, Deadline, DeferCancelableThrough, DeferThrough, DeferredReply, Effect,
        Isolate, IsolateId, Outbound, PendingCallSet, PendingCallSetInsertError, RequestCall,
        RequestContext, RequestDeferCancelableThrough, RequestDeferThrough, RequestEffect,
        RestartableChildDefinition, Shard, ShardId, SingleShard, SpawnObservedError, batch, fail,
        isolate, isolate_types, noop, reply, reply_to, reply_to_request, restart_children, send,
        sequence, spawn, spawn_observed, stop, stop_children, stop_with,
        time::{
            Backoff, BackoffDelay, IntervalDelay, MissedTickPolicy, RecurringCatchUp,
            RecurringTick, RecurringTickDecision, RecurringTickReport, RecurringTickStale,
            RecurringTickToken, TimerConfigError, TimerDecision, TimerInterval,
        },
    };
}

pub mod capacity;
mod pending_call_set;
pub mod pool;
pub mod time;
pub use pending_call_set::{PendingCallSet, PendingCallSetInsertError};
